// SPDX-License-Identifier: MPL-2.0
//! What the Inspector's pickers need from the rest of the editor and what they ask of it
//! (spec addendum § 7.10-7.12, ADR-7786).
//!
//! A drawer runs without the world (it edits a clone of the component), so each component draw
//! gets a [`PickerLinks`] snapshot ([`links`]): the Content Browser's selected asset (**Use
//! selected**), the cached referencer lists of asset paths (the shared-asset indicator), the
//! saved swatches and recent colors (the color picker) and the armed entity eyedropper. The
//! drawer answers with [`PickerRequest`]s that [`run_requests`] carries out once the draw is
//! over: reveal an asset in the Content Browser, count an asset's referencers, make an asset
//! unique, save or remove a swatch, remember a recent color, arm the eyedropper.
//!
//! Referencer lists are computed once per content-index generation (a file change, an import or
//! a delete bumps it), never per frame: the first draw that shows a field asks, the next frame
//! shows the count.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_ecs::prelude::*;

use super::color;
use crate::editor_prefs::{EditorPreferences, PrefValue};

/// The preference that holds the saved swatches (`#rrggbbaa` list).
pub const SWATCHES_PREF: &str = "color.swatches";
/// The preference that holds the recent colors (`#rrggbbaa` list, newest first).
pub const RECENT_PREF: &str = "color.recent";

/// The Content Browser's selected asset (project-relative), kept current by
/// [`super::refresh_asset_catalog`]; what **Use selected** and `editor.asset_field_set`'s
/// `from_selection` take.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentSelection(pub Option<String>);

/// Referencer lists by asset path, for one content-index generation.
#[derive(Resource, Debug, Clone, Default)]
pub struct ReferencerCache {
    /// The content-index generation the lists belong to.
    pub generation: u64,
    /// Asset path → the project files that reference it (sorted, the asset itself excluded).
    pub by_path: Arc<BTreeMap<String, Vec<String>>>,
    /// Lists computed so far (a scan of the project each); tests read it.
    pub computed: u64,
}

/// The entity eyedropper of one entity field: the next entity picked in the viewport or the
/// Outliner is written to the field, and the selection goes back to what it was.
#[derive(Resource, Debug, Clone)]
pub struct EntityEyedropper {
    /// The entity whose component holds the field.
    pub entity: Entity,
    /// The component's type path.
    pub component: String,
    /// The field's reflect path.
    pub path: String,
    /// The selection when the eyedropper was armed.
    pub selection: crate::selection::Selection,
}

/// What the drawer asked for this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRequest {
    /// Reveal the asset in the Content Browser.
    BrowseTo(String),
    /// Count the referencers of an asset path (shown from the next frame).
    Referencers(String),
    /// Copy the asset of the field at this reflect path to a new file and repoint the field.
    MakeUnique(String),
    /// Save a linear color as a swatch.
    SaveSwatch([f32; 4]),
    /// Remove the saved swatch at this index.
    RemoveSwatch(usize),
    /// Remember a linear color as recent.
    Recent([f32; 4]),
    /// Arm the entity eyedropper for the field at this reflect path.
    ArmEyedropper(String),
    /// Disarm the eyedropper.
    CancelEyedropper,
    /// An inline asset section's request (open, edit, save; ADR-8107).
    Inline(super::inline_asset::InlineRequest),
    /// Arm the color eyedropper for the color field at this reflect path (ADR-8106).
    ArmColorEyedropper(String),
    /// Disarm the color eyedropper.
    CancelColorEyedropper,
}

/// The snapshot a component draw sees, and the requests it makes.
#[derive(Debug, Clone, Default)]
pub struct PickerLinks {
    /// The Content Browser's selected asset.
    pub selected_asset: Option<String>,
    /// Referencer lists of this content-index generation.
    pub referencers: Arc<BTreeMap<String, Vec<String>>>,
    /// Saved swatches (linear RGBA).
    pub swatches: Vec<[f32; 4]>,
    /// Recent colors (linear RGBA), newest first.
    pub recent: Vec<[f32; 4]>,
    /// The reflect path of the field whose eyedropper is armed, when it belongs to the component
    /// being drawn.
    pub eyedropper: Option<String>,
    /// The inline asset documents (`super::inline_asset`, ADR-8107).
    pub inline: super::inline_asset::InlineViews,
    /// The reflect path of the color field whose eyedropper is armed, when it belongs to the
    /// component being drawn (ADR-8106).
    pub color_eyedropper: Option<String>,
    requests: Vec<PickerRequest>,
}

impl PickerLinks {
    /// Adds a request (an equal pending one is not repeated).
    pub fn request(&mut self, request: PickerRequest) {
        if !self.requests.contains(&request) {
            self.requests.push(request);
        }
    }

    /// Takes the requests made so far.
    pub fn take_requests(&mut self) -> Vec<PickerRequest> {
        std::mem::take(&mut self.requests)
    }

    /// The referencers of `path` when counted this generation; asks for them otherwise.
    pub fn referencers_of(&mut self, path: &str) -> Option<usize> {
        if path.is_empty() {
            return None;
        }
        match self.referencers.get(path) {
            Some(found) => Some(found.len()),
            None => {
                self.request(PickerRequest::Referencers(path.to_owned()));
                None
            }
        }
    }
}

/// The current content-index generation (0 without an index).
fn index_generation(world: &World) -> u64 {
    world
        .get_resource::<crate::content_index::ContentIndex>()
        .map_or(0, |index| index.generation)
}

/// The snapshot for drawing `component` of `entity`.
#[must_use]
pub fn links(world: &World, entity: Option<Entity>, component: &str) -> PickerLinks {
    let prefs = world.get_resource::<EditorPreferences>();
    let list = |key: &str| {
        prefs
            .map(|p| color::parse_hex_list(&p.text(key)))
            .unwrap_or_default()
    };
    let referencers = world
        .get_resource::<ReferencerCache>()
        .filter(|cache| cache.generation == index_generation(world))
        .map(|cache| Arc::clone(&cache.by_path))
        .unwrap_or_default();
    let eyedropper = world
        .get_resource::<EntityEyedropper>()
        .filter(|armed| Some(armed.entity) == entity && armed.component == component)
        .map(|armed| armed.path.clone());
    PickerLinks {
        selected_asset: world
            .get_resource::<ContentSelection>()
            .and_then(|selection| selection.0.clone()),
        referencers,
        swatches: list(SWATCHES_PREF),
        recent: list(RECENT_PREF),
        eyedropper,
        inline: super::inline_asset::views(world),
        color_eyedropper: super::color_eyedropper::armed_path(world, entity, component),
        requests: Vec::new(),
    }
}

/// The project files that reference `path`, cached for the current content-index generation.
/// Without an open project the list is empty.
pub fn referencers(world: &mut World, path: &str) -> Vec<String> {
    let generation = index_generation(world);
    let mut cache = world.remove_resource::<ReferencerCache>().unwrap_or_default();
    if cache.generation != generation {
        cache.generation = generation;
        cache.by_path = Arc::default();
    }
    let found = match cache.by_path.get(path) {
        Some(found) => found.clone(),
        None => {
            let _span = tracing::info_span!("inspector_referencers", path).entered();
            let found = crate::mesh_source::project_root(world)
                .map(|root| {
                    let mut found: Vec<String> = crate::content::find_referencers(&root, path)
                        .into_iter()
                        .filter(|file| file != path)
                        .collect();
                    found.sort();
                    found.dedup();
                    found
                })
                .unwrap_or_default();
            Arc::make_mut(&mut cache.by_path).insert(path.to_owned(), found.clone());
            cache.computed += 1;
            found
        }
    };
    world.insert_resource(cache);
    found
}

/// Changes the color list preference `key` with `edit` (unchanged when the preference is not
/// registered).
fn edit_color_list(world: &mut World, key: &str, edit: impl FnOnce(&mut Vec<[f32; 4]>)) {
    let mut prefs = world.get_resource_or_insert_with(EditorPreferences::default);
    let mut list = color::parse_hex_list(&prefs.text(key));
    edit(&mut list);
    if let Err(err) = prefs.set(key, &PrefValue::Text(color::hex_list(&list))) {
        tracing::warn!(target: "regia_editor", "{key}: {err}");
    }
}

/// Saves a linear color as a swatch (first in the list; an equal swatch moves up).
pub fn save_swatch(world: &mut World, rgba: [f32; 4]) {
    edit_color_list(world, SWATCHES_PREF, |list| {
        color::push_front_unique(list, rgba, color::MAX_SWATCHES);
    });
}

/// Removes the saved swatch at `index` (nothing when out of range).
pub fn remove_swatch(world: &mut World, index: usize) {
    edit_color_list(world, SWATCHES_PREF, |list| {
        if index < list.len() {
            list.remove(index);
        }
    });
}

/// Remembers a linear color as the newest recent color.
pub fn remember_recent(world: &mut World, rgba: [f32; 4]) {
    edit_color_list(world, RECENT_PREF, |list| {
        color::push_front_unique(list, rgba, color::MAX_RECENT);
    });
}

/// Carries out what the draw of `component` of `entity` asked for. Call it after the draw, and
/// not for a pass egui discards (the same clicks arrive again in the pass it keeps).
pub fn run_requests(world: &mut World, entity: Entity, component: &str, requests: Vec<PickerRequest>) {
    for request in requests {
        match request {
            PickerRequest::BrowseTo(path) => {
                world
                    .get_resource_or_insert_with(crate::asset_panels::AssetPanels::default)
                    .select(path);
            }
            PickerRequest::Referencers(path) => {
                let _ = referencers(world, &path);
            }
            PickerRequest::MakeUnique(path) => {
                if let Err(err) = super::make_unique::make_unique(world, entity, component, &path) {
                    tracing::warn!(target: "regia_editor", "make unique: {}", err.message);
                }
            }
            PickerRequest::SaveSwatch(rgba) => save_swatch(world, rgba),
            PickerRequest::RemoveSwatch(index) => remove_swatch(world, index),
            PickerRequest::Recent(rgba) => remember_recent(world, rgba),
            PickerRequest::ArmEyedropper(path) => {
                let selection = world
                    .get_resource::<crate::selection::Selection>()
                    .cloned()
                    .unwrap_or_default();
                world.insert_resource(EntityEyedropper {
                    entity,
                    component: component.to_owned(),
                    path,
                    selection,
                });
            }
            PickerRequest::CancelEyedropper => {
                world.remove_resource::<EntityEyedropper>();
            }
            PickerRequest::Inline(request) => super::inline_asset::run_request(world, request),
            PickerRequest::ArmColorEyedropper(path) => {
                super::color_eyedropper::arm(world, vec![entity], component, &path);
            }
            PickerRequest::CancelColorEyedropper => super::color_eyedropper::cancel(world),
        }
    }
}

/// Finishes an armed entity eyedropper: Esc disarms it; a selection change (a click in the
/// viewport or the Outliner) writes the newly selected entity to the field as one undo step and
/// puts the selection back. Returns the entity written. The Inspector calls it once per frame
/// before drawing.
pub fn resolve_eyedropper(world: &mut World, escape: bool) -> Option<Entity> {
    let armed = world.get_resource::<EntityEyedropper>()?.clone();
    if escape || world.get_entity(armed.entity).is_err() {
        world.remove_resource::<EntityEyedropper>();
        return None;
    }
    let now = world
        .get_resource::<crate::selection::Selection>()
        .cloned()
        .unwrap_or_default();
    if now.entities() == armed.selection.entities() {
        return None;
    }
    world.remove_resource::<EntityEyedropper>();
    world.insert_resource(armed.selection.clone());
    let picked = now.primary()?;
    match super::multi::edit_value(
        world,
        &[armed.entity],
        &armed.component,
        &armed.path,
        Box::new(picked),
    ) {
        Ok(_) => Some(picked),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "eyedropper: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swatches_and_recent_colors_live_in_the_preferences() {
        let mut world = World::new();
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 1.0];
        save_swatch(&mut world, red);
        save_swatch(&mut world, blue);
        save_swatch(&mut world, red);
        remember_recent(&mut world, blue);
        let snapshot = links(&world, None, "");
        assert_eq!(snapshot.swatches, vec![red, blue]);
        assert_eq!(snapshot.recent, vec![blue]);
        remove_swatch(&mut world, 0);
        remove_swatch(&mut world, 9);
        assert_eq!(links(&world, None, "").swatches, vec![blue]);
        let prefs = world.resource::<EditorPreferences>();
        assert_eq!(prefs.text(SWATCHES_PREF), "#0000ffff");
    }

    #[test]
    fn a_request_is_made_once_per_draw() {
        let mut links = PickerLinks::default();
        assert_eq!(links.referencers_of("assets/a.png"), None);
        assert_eq!(links.referencers_of("assets/a.png"), None);
        assert_eq!(links.referencers_of(""), None);
        assert_eq!(
            links.take_requests(),
            vec![PickerRequest::Referencers("assets/a.png".to_owned())]
        );
    }
}
