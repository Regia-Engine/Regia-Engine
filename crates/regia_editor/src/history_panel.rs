// SPDX-License-Identifier: MPL-2.0
//! Shared undo history: chronological entries, jump-to boundaries and command details.
//! Open through Window → Undo History or `editor.layout_apply { open: "undo_history" }`.

use regia_ecs::prelude::*;

use crate::command::{HistoryEntry, UndoStack};

#[derive(Resource, Default)]
struct HistoryPanel {
    selected: usize,
    acted_frame: Option<u64>,
    error: Option<String>,
}

/// Draws the history only when its dock tab is visible. Buttons use the current theme and
/// point sizes, so appearance scaling and ordinary Tab/Enter/Space navigation apply.
pub fn history_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!("undo_history_panel").entered();
    let (entries, current, transaction) = world.get_resource::<UndoStack>().map_or_else(
        || (Vec::new(), 0, None),
        |stack| {
            (
                stack.history(),
                stack.undo_depth(),
                stack.transaction_label().map(str::to_owned),
            )
        },
    );
    let mut panel = world.remove_resource::<HistoryPanel>().unwrap_or_default();
    panel.selected = panel.selected.min(entries.len());
    ui.label(format!(
        "{} applied · {} available to redo",
        current,
        entries.len() - current
    ));
    if let Some(label) = &transaction {
        ui.label(format!("Transaction in progress: {label}"));
    }
    let mut requested = None;
    egui::ScrollArea::vertical()
        .id_salt("undo_history_entries")
        .show(ui, |ui| {
            for index in 0..=entries.len() {
                let text = if index == 0 {
                    "0 · Before retained history".to_owned()
                } else {
                    let entry = &entries[index - 1];
                    format!(
                        "{index} · {} · {}{}",
                        entry.description,
                        entry.documents.join(", "),
                        if index == current {
                            " · Current"
                        } else if index > current {
                            " · Redo"
                        } else {
                            ""
                        }
                    )
                };
                let response = ui
                    .push_id(index, |ui| {
                        crate::agent_parity::action_enabled(
                            ui,
                            transaction.is_none(),
                            &crate::agent_parity::HISTORY_JUMP,
                            Some(text),
                        )
                    })
                    .inner;
                if response.has_focus() || response.hovered() {
                    panel.selected = index;
                }
                if response.clicked() {
                    panel.selected = index;
                    requested = Some(index);
                }
            }
        });
    ui.separator();
    ui.heading("Change details");
    if let Some(entry) = panel.selected.checked_sub(1).and_then(|index| entries.get(index)) {
        details(ui, entry);
    } else {
        ui.label("The state before the oldest retained edit.");
    }
    // Egui may request a second layout pass: mutations happen once per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    if panel.acted_frame != Some(frame)
        && let Some(index) = requested
    {
        panel.acted_frame = Some(frame);
        panel.error = crate::command::jump_world(world, index)
            .err()
            .map(|error| error.to_string());
    }
    if let Some(error) = &panel.error {
        ui.label(error);
    }
    world.insert_resource(panel);
}

fn details(ui: &mut egui::Ui, entry: &HistoryEntry) {
    ui.label(&entry.description);
    if entry.touched.is_empty() {
        ui.label("This command does not report detailed changes.");
    }
    for touch in &entry.touched {
        ui.label(format!("Document: {}", touch.document));
        if let Some(entity) = touch.entity {
            ui.label(format!("Entity: {entity}"));
        }
        if let Some(guid) = &touch.guid {
            ui.label(format!("Entity guid: {guid}"));
        }
        if let Some(asset) = &touch.asset {
            ui.label(format!("Asset: {asset}"));
        }
        if let Some(property) = &touch.property {
            ui.label(format!("Property: {property}"));
        }
    }
}

#[derive(Resource, Default)]
struct OpenFrame(Option<u64>);

/// Claims the Window-menu action once per egui frame, including repeated layout passes.
pub(crate) fn open_once(world: &mut World, frame: u64) -> bool {
    let mut last = world.get_resource_or_insert_with::<OpenFrame>(Default::default);
    if last.0 == Some(frame) {
        return false;
    }
    last.0 = Some(frame);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::EditorCommand;

    #[derive(Resource, Default)]
    struct Count(usize);
    struct Edit;
    impl EditorCommand for Edit {
        fn apply(&mut self, world: &mut World) {
            world.resource_mut::<Count>().0 += 1;
        }
        fn revert(&mut self, world: &mut World) {
            world.resource_mut::<Count>().0 -= 1;
        }
        fn merge(&mut self, _: &dyn EditorCommand) -> bool {
            false
        }
        fn description(&self) -> String {
            "Count edit".into()
        }
    }

    #[test]
    fn keyboard_tab_enter_jumps_without_a_pointer_at_two_scales() {
        for scale in [1.0, 1.75] {
            let context = egui::Context::default();
            context.set_zoom_factor(scale);
            let mut world = World::new();
            world.insert_resource(Count(1));
            let mut stack = UndoStack::new();
            stack.push(Box::new(Edit));
            world.insert_resource(stack);
            use crate::canvas_keys::test_input::{frame, key, tab_to};
            frame(&context, [600.0, 400.0], vec![], |ui| {
                history_panel(ui, &mut world)
            });
            tab_to(
                &context,
                [600.0, 400.0],
                "0 · Before retained history",
                false,
                8,
                |ui| history_panel(ui, &mut world),
            );
            frame(
                &context,
                [600.0, 400.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| history_panel(ui, &mut world),
            );
            assert_eq!(world.resource::<Count>().0, 0, "Enter jumps to boundary zero");
            assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
            tab_to(
                &context,
                [600.0, 400.0],
                "1 · Count edit · level · Redo",
                false,
                8,
                |ui| history_panel(ui, &mut world),
            );
            frame(
                &context,
                [600.0, 400.0],
                vec![key(egui::Key::Space, egui::Modifiers::NONE)],
                |ui| history_panel(ui, &mut world),
            );
            assert_eq!(world.resource::<Count>().0, 1, "Space redoes the entry");
        }
    }
}
