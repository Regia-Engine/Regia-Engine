// SPDX-License-Identifier: MPL-2.0
//! The preview language (ADR-7855, parity row P-279, `book/agents/editor-localization.md`): one locale picker in
//! the main toolbar that sets the locale the edit viewport's UI and the play session show, with **Source** to
//! reset. It is `ui.set_locale` on the runtime `Localization` of both worlds, kept in step every frame, so a
//! play session started after the pick shows the same language.
//!
//! The pick is an editor-session setting ([`PreviewLocale`], an edit-world resource); it changes no file. While a
//! locale other than the source previews, the text fields of localized nodes in the UI Designer are read-only
//! ([`is_previewing`]): the text on screen is a translation, and edits to the source text belong to Source.

use regia_ecs::prelude::*;
use regia_ui::localization::Localization;

use super::LocPanel;
use super::model::err;
use crate::data_panels::PanelError;

/// The locale the viewport and play preview (`None`: the source locale, the game's own choice).
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub struct PreviewLocale {
    /// The previewed locale tag; `None` is Source.
    pub locale: Option<String>,
}

/// Whether the play world shows a preview locale now (an edit-world resource). The toolbar picker and
/// `editor.preview_locale` never see the play world, so a pick of Source leaves this set and the next
/// [`sync_frame`] with the play world puts it back on the source locale once (review g17 row 1).
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
struct PlayShowsPreview(bool);

/// Records whether the play world shows a preview locale now.
fn mark_play(world: &mut World, shows: bool) {
    if shows || world.contains_resource::<PlayShowsPreview>() {
        world.insert_resource(PlayShowsPreview(shows));
    }
}

/// Whether the play world was left showing a preview locale.
fn play_marked(world: &World) -> bool {
    world.get_resource::<PlayShowsPreview>().is_some_and(|m| m.0)
}

/// What a preview pick did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewOutcome {
    /// The preview locale now (`None`: Source).
    pub locale: Option<String>,
    /// The project's source locale.
    pub source: String,
    /// The locales the picker offers.
    pub locales: Vec<String>,
    /// The edit world's `Localization` now shows the pick.
    pub edit_world: bool,
    /// The play world's `Localization` shows the pick now (`false`: no play session, or none has one; a
    /// play session started later follows the pick when the editor draws).
    pub play_world: bool,
}

/// The project's source locale: the Localization panel's table when it has read one, else `en`.
#[must_use]
pub fn source_locale(world: &World) -> String {
    world
        .get_resource::<LocPanel>()
        .and_then(LocPanel::table)
        .map_or_else(|| "en".to_owned(), |t| t.source.clone())
}

/// The locales the picker offers: the project's (the Localization panel's table), then any more the edit
/// world's runtime `Localization` has loaded; sorted, without repeats.
#[must_use]
pub fn available_locales(world: &World) -> Vec<String> {
    let mut out: Vec<String> = world
        .get_resource::<LocPanel>()
        .and_then(LocPanel::table)
        .map(|t| t.locales.clone())
        .unwrap_or_default();
    if let Some(loc) = world.get_resource::<Localization>() {
        out.extend(loc.available.iter().cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// The preview locale now (`None`: Source).
#[must_use]
pub fn current(world: &World) -> Option<String> {
    world
        .get_resource::<PreviewLocale>()
        .and_then(|p| p.locale.clone())
}

/// Switches the `Localization` of `world` to `locale` (or its nearest loaded parent); whether it has one.
fn apply(world: &mut World, locale: &str) -> bool {
    match world.get_resource_mut::<Localization>() {
        Some(mut loc) => {
            if loc.locale() != locale {
                loc.set_locale(locale);
            }
            true
        }
        None => false,
    }
}

/// Sets the preview locale: `None` or the source locale is Source. The edit world's `Localization` follows now,
/// `play`'s too when given; otherwise the play world follows the next time the editor draws ([`sync_frame`]),
/// Source included: a play session left on a preview goes back to the source locale once.
///
/// # Errors
///
/// `loc_panel_locale_not_found` when the project has no such locale.
pub fn set_preview_locale(
    world: &mut World,
    play: Option<&mut World>,
    locale: Option<&str>,
) -> Result<PreviewOutcome, PanelError> {
    let source = source_locale(world);
    let locales = available_locales(world);
    let pick = locale.map(str::trim).filter(|l| !l.is_empty());
    if let Some(l) = pick
        && !locales.iter().any(|known| known == l)
    {
        return Err(err(
            "locale_not_found",
            format!("no locale `{l}`; locales: {}", locales.join(", ")),
        ));
    }
    let previewing = pick.filter(|l| *l != source).map(str::to_owned);
    world.insert_resource(PreviewLocale {
        locale: previewing.clone(),
    });
    let target = previewing.as_deref().unwrap_or(&source);
    let edit_world = apply(world, target);
    let play_world = match play {
        Some(p) => {
            let shown = apply(p, target);
            mark_play(world, shown && previewing.is_some());
            shown
        }
        // No play world here: the next `sync_frame` with one applies the pick, or Source once.
        None => false,
    };
    Ok(PreviewOutcome {
        locale: previewing,
        source,
        locales,
        edit_world,
        play_world,
    })
}

/// The egui id of the "a non-source locale previews" flag [`sync_frame`] stores for the panels.
fn flag_id() -> egui::Id {
    egui::Id::new("regia_preview_locale_active")
}

/// Whether a locale other than the source previews now (the UI Designer's localized text fields lock).
#[must_use]
pub fn is_previewing(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(flag_id())).unwrap_or(false)
}

/// Keeps both worlds on the preview locale and publishes [`is_previewing`]. Runs once per frame from the
/// editor shell with the play world while a session runs; a world with no `Localization` is left alone. On
/// Source, a play world still showing a preview (Source was picked where the play world was not at hand: the
/// toolbar, the agent) goes back to the source locale once; later frames on Source touch nothing (the game
/// keeps its own locale choice), and a frame with no play session forgets the preview, so the next session is
/// left alone too.
pub fn sync_frame(ctx: &egui::Context, world: &mut World, play: Option<&mut World>) {
    let Some(locale) = current(world) else {
        ctx.data_mut(|d| d.insert_temp(flag_id(), false));
        if play_marked(world) {
            if let Some(play) = play {
                let source = source_locale(world);
                apply(play, &source);
            }
            mark_play(world, false);
        }
        return;
    };
    ctx.data_mut(|d| d.insert_temp(flag_id(), true));
    apply(world, &locale);
    let shown = play.is_some_and(|play| apply(play, &locale));
    mark_play(world, shown);
}

/// Records a widget's id for the keyboard test.
#[cfg(test)]
fn record_id(ui: &egui::Ui, key: &str, id: egui::Id) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("preview-test-id", key)), id));
}

/// The toolbar's locale picker: a drop-down of Source and the project's locales; a pick is applied through
/// [`set_preview_locale`] (the play world follows on the next [`sync_frame`]).
pub fn preview_locale_picker(ui: &mut egui::Ui, world: &mut World) {
    // The panel's table is the locale list; read it once when the project is open and nothing has yet.
    if world
        .get_resource::<LocPanel>()
        .and_then(LocPanel::table)
        .is_none()
    {
        let _ = super::with_panel(world, |_, _| Ok(()));
    }
    let locales = available_locales(world);
    let source = source_locale(world);
    let current = current(world);
    let shown = current.clone().unwrap_or_else(|| "Source".to_owned());
    let mut pick: Option<Option<String>> = None;
    let combo = egui::ComboBox::from_label("Preview language")
        .selected_text(shown)
        .show_ui(ui, |ui| {
            let r = ui.selectable_label(current.is_none(), format!("Source ({source})"));
            #[cfg(test)]
            record_id(ui, "item:source", r.id);
            if r.clicked() {
                pick = Some(None);
            }
            for l in locales.iter().filter(|l| **l != source) {
                let r = ui.selectable_label(current.as_deref() == Some(l.as_str()), l);
                #[cfg(test)]
                record_id(ui, &format!("item:{l}"), r.id);
                if r.clicked() {
                    pick = Some(Some(l.clone()));
                }
            }
        });
    #[cfg(test)]
    record_id(ui, "combo", combo.response.id);
    combo
        .response
        .on_hover_text("The language the viewport UI and play show; Source shows the game's own");
    if let Some(pick) = pick {
        let _ = set_preview_locale(world, None, pick.as_deref());
    }
}
