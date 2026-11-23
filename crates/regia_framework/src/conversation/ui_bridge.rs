// SPDX-License-Identifier: MPL-2.0
//! Shows the `regia_ui` choice widget for a conversation `Choice` step and feeds the pick back
//! (`docs/specs/56-dialogue-and-subtitles.md` § 12, ADR-3234).
//!
//! [`ConversationChoiceBridgePlugin`] connects two existing pieces and owns no widget logic:
//! - `ConversationChoiceRequested` (the runner offered options) opens a
//!   [`regia_ui::choice::ChoiceWidget`] on the [`ChoiceBoard`]; the right-to-left mirror follows
//!   the active locale;
//! - `ChoicePicked` (the player or the timer picked) calls `ConversationRunner::choose` for the
//!   run that asked.
//!
//! A conversation choice cannot be dismissed: a cancel re-opens the same widget.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_ui::captions::is_rtl_locale;
use regia_ui::choice::{ChoiceBoard, ChoiceLayout, ChoiceOption, ChoicePicked, ChoiceWidget};
use regia_ui::localization::Localization;

use super::runner::{ChoiceView, RunId};
use super::{
    ConversationChoiceRequested, ConversationLibrary, ConversationRunner, WorldEnv, with_localization,
    with_voice_player,
};
use regia_ui::captions::CaptionClock;

/// How the bridge shows choices (a game sets this resource).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct ConversationChoiceUi {
    /// List or wheel.
    pub layout: ChoiceLayout,
    /// Fixed steps until the default is picked; `None` waits for the player.
    pub timer_steps: Option<u32>,
    /// The option the timer picks.
    pub default_index: usize,
    pending: BTreeMap<u64, Pending>,
    ready: Vec<(RunId, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    run: RunId,
    prompt: Option<String>,
    options: Vec<ChoiceView>,
}

impl Default for ConversationChoiceUi {
    fn default() -> Self {
        Self {
            layout: ChoiceLayout::List,
            timer_steps: None,
            default_index: 0,
            pending: BTreeMap::new(),
            ready: Vec::new(),
        }
    }
}

impl ConversationChoiceUi {
    /// The runs waiting for a pick, with their widget ids.
    pub fn waiting(&self) -> Vec<(u64, RunId)> {
        self.pending.iter().map(|(w, p)| (*w, p.run)).collect()
    }

    fn open(&mut self, board: &mut ChoiceBoard, pending: Pending, rtl: bool) {
        let options = pending
            .options
            .iter()
            .map(|o| ChoiceOption::new(o.text.clone()))
            .collect();
        let mut widget = ChoiceWidget::open(self.layout, pending.prompt.clone(), options, rtl);
        if let Some(steps) = self.timer_steps {
            widget = widget.with_timer(steps, self.default_index);
        }
        let id = board.show(widget);
        self.pending.insert(id, pending);
    }
}

/// `PostUpdate`: opens a widget for each `ConversationChoiceRequested`.
pub fn open_choice_widgets(
    mut requests: MessageReader<ConversationChoiceRequested>,
    mut board: ResMut<ChoiceBoard>,
    mut ui: ResMut<ConversationChoiceUi>,
    loc: Option<Res<Localization>>,
) {
    let rtl = loc.is_some_and(|l| is_rtl_locale(l.locale()));
    for r in requests.read() {
        let pending = Pending {
            run: r.run,
            prompt: r.prompt.clone(),
            options: r.options.clone(),
        };
        ui.open(&mut board, pending, rtl);
    }
}

/// `PostUpdate`: moves `ChoicePicked` into the ready list (a cancel re-opens the widget).
pub fn collect_choice_picks(
    mut picks: MessageReader<ChoicePicked>,
    mut board: ResMut<ChoiceBoard>,
    mut ui: ResMut<ConversationChoiceUi>,
    loc: Option<Res<Localization>>,
) {
    let rtl = loc.is_some_and(|l| is_rtl_locale(l.locale()));
    for p in picks.read() {
        let Some(pending) = ui.pending.remove(&p.widget) else {
            continue;
        };
        match p.index {
            Some(index) => ui.ready.push((pending.run, index)),
            None => ui.open(&mut board, pending, rtl),
        }
    }
}

/// `PostUpdate`, before the runner: tells the runner which option was picked.
pub fn apply_choice_picks(world: &mut World) {
    let ready = match world.get_resource_mut::<ConversationChoiceUi>() {
        Some(mut ui) if !ui.ready.is_empty() => std::mem::take(&mut ui.ready),
        _ => return,
    };
    let now = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    world.resource_scope(|world, mut runner: Mut<ConversationRunner>| {
        world.resource_scope(|world, library: Mut<ConversationLibrary>| {
            with_localization(world, |world, loc| {
                with_voice_player(world, now, |voice| {
                    let env = WorldEnv {
                        library: &library,
                        loc,
                        voice,
                    };
                    for (run, index) in ready {
                        // A run stopped meanwhile answers with an error; there is nothing to do.
                        if let Err(e) = runner.choose(run, index, &env) {
                            tracing::debug!(run = run.0, index, error = %e, "conversation choice not applied");
                        }
                    }
                })
            })
        })
    });
}

/// Registers the bridge. Needs `ChoicePlugin` (added by `UiPlugin`) and `ConversationPlugin`.
pub struct ConversationChoiceBridgePlugin;

impl Plugin for ConversationChoiceBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConversationChoiceUi>().add_systems(
            PostUpdate,
            (
                open_choice_widgets.after(super::conversation_system),
                (collect_choice_picks, apply_choice_picks)
                    .chain()
                    .before(super::conversation_system),
            ),
        );
    }
}

#[cfg(test)]
mod tests;
