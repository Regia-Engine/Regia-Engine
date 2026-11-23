// SPDX-License-Identifier: MPL-2.0
//! The **Restore on stop** row of the Sequence Curves panel's sequence properties (ADR-7726,
//! `book/agents/seq-curve-panel.md`): the `.seq.ron` v5 root flag `restore_on_stop` (ADR-2948).
//! With it set, a player that stops (or lets go, or despawns) writes back the value each keyed
//! property had before its first write; without it the last sampled values stay. The panel's
//! toggle and `editor.seq_restore_on_stop` run [`set_restore_on_stop`]: one undo entry per change,
//! through the same authoring path as the track edits, so Save writes the flag and a reload (and
//! the runtime player made from the file) reads it back.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::sequencer::SequencePlayer;
use regia_framework::sequencer::edit::EditError;
use serde_json::{Value, json};

use crate::seq_curve_panel::{SeqCurvePanel, player_of, rpc};
use crate::viewport_agent::Handler;

/// Sets the panel's sequence's `restore_on_stop` to `on`, as one undo entry (nothing when it
/// already is). Answers whether it changed. Errors: `seq_curve_no_player`.
///
/// # Errors
/// `seq_curve_no_player` when the panel has no sequence player.
pub fn set_restore_on_stop(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    on: bool,
) -> Result<bool, EditError> {
    let current = player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.sequence().restore_on_stop);
    if current == Some(on) {
        return Ok(false);
    }
    let label = if on {
        "Restore sequence values on stop"
    } else {
        "Keep sequence values on stop"
    };
    let ((), command) = super::author(world, panel, label, |seq, _| {
        seq.restore_on_stop = on;
        Ok(())
    })?;
    super::push(world, command);
    Ok(true)
}

/// The panel's sequence's flag, when it has a player.
#[must_use]
pub fn restore_on_stop(world: &mut World, panel: &SeqCurvePanel) -> Option<bool> {
    player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.sequence().restore_on_stop)
}

/// The row: a toggle bound to the flag.
pub fn row(ui: &mut egui::Ui, world: &mut World, panel: &mut SeqCurvePanel) -> Result<(), EditError> {
    let Some(mut on) = restore_on_stop(world, panel) else {
        return Ok(());
    };
    let mut result = Ok(());
    ui.horizontal(|ui| {
        if crate::agent_parity::action_toggle(ui, &crate::agent_parity::SEQ_RESTORE_ON_STOP, &mut on)
            .on_hover_text(
                "When the sequence stops, put back the values its tracks overwrote (the file's restore_on_stop)",
            )
            .changed()
        {
            result = set_restore_on_stop(world, panel, on).map(|_| ());
        }
    });
    result
}

/// `editor.seq_restore_on_stop { value, player? }`.
fn seq_restore_on_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params
        .get("value")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`value` (true or false) is required"))?;
    super::with_panel(world, params, |world, panel| {
        let changed = set_restore_on_stop(world, panel, on).map_err(rpc)?;
        Ok(json!({
            "player": player_of(world, panel).map(Entity::to_bits),
            "restore_on_stop": restore_on_stop(world, panel),
            "changed": changed,
        }))
    })
}

/// The agent method of the row.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.seq_restore_on_stop",
            tool: "seq_restore_on_stop",
            description: "Set the Sequence Curves panel's sequence's restore_on_stop (the panel's Restore on stop \
                toggle): with it, a player that stops puts back the values its tracks overwrote. One undo entry \
                per change; Save writes it to the .seq.ron.",
            params: &[
                ParamSchema {
                    name: "value",
                    kind: "boolean",
                    required: true,
                    description: "The new flag.",
                },
                ParamSchema {
                    name: "player",
                    kind: "number",
                    required: false,
                    description: "Sequence player entity id (default: the panel's player).",
                },
            ],
            result: "{ player, restore_on_stop, changed }",
            mutating: true,
            permission: Permission::EditScene,
        },
        seq_restore_on_stop as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::UndoStack;
    use regia::prelude::*;
    use regia_framework::sequencer::{
        Binding, EndPolicy, Sequence, SequenceBindings, SequencePlayer, SequencerPlugin,
    };
    use regia_scene::prelude::Transform;

    const SEQ: &str = "assets/cine/door.seq.ron";
    const TEXT: &str = r#"(version: 5, name: "door", fps: 10.0, tracks: [
        (name: "door", target: "door", property: "transform.position",
         keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 1.0, value: (5.0, 0.0, 0.0))])),
    ])"#;

    fn project() -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("seq-restore-row");
        let path = root.join(SEQ);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        regia_test_utils::write_atomic(&path, TEXT.as_bytes()).expect("file");
        root
    }

    /// Acceptance (2): set in the panel (the row's setter and the agent method), one undo step,
    /// saved, reloaded, and read back by a runtime player made from the file.
    #[test]
    fn the_flag_is_one_undo_step_and_round_trips_through_the_file() {
        let root = project();
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut panel = SeqCurvePanel::default();
        let player = crate::seq_curve_panel::open_sequence(&mut world, &mut panel, SEQ).expect("open");
        assert_eq!(restore_on_stop(&mut world, &panel), Some(false));
        assert!(set_restore_on_stop(&mut world, &mut panel, true).expect("set"));
        assert!(
            !set_restore_on_stop(&mut world, &mut panel, true).expect("same"),
            "no-op"
        );
        assert_eq!(restore_on_stop(&mut world, &panel), Some(true));
        let depth = world.resource::<UndoStack>().undo_depth();
        assert_eq!(depth, 1, "one undo entry");
        world.resource_scope(|world, mut undo: Mut<UndoStack>| assert!(undo.undo(world)));
        assert_eq!(restore_on_stop(&mut world, &panel), Some(false), "undone");
        world.resource_scope(|world, mut undo: Mut<UndoStack>| assert!(undo.redo(world)));
        assert_eq!(restore_on_stop(&mut world, &panel), Some(true), "redone");
        crate::seq_curve_panel::save_sequence(&mut world, &mut panel, None).expect("save");
        let text = std::fs::read_to_string(root.join(SEQ)).expect("saved");
        let saved = Sequence::from_ron(&text).expect("parses");
        assert!(saved.restore_on_stop, "{text}");
        assert!(
            SequencePlayer::new(saved.clone()).restore_on_stop,
            "the runtime player reads it"
        );
        // Reloaded in another panel: the row shows it.
        let mut other = SeqCurvePanel::default();
        crate::seq_curve_panel::open_sequence(&mut world, &mut other, SEQ).expect("reopen");
        assert_eq!(restore_on_stop(&mut world, &other), Some(true));
        // The agent method clears it, as one more undo entry, and Save writes the default back.
        world.insert_resource(panel);
        let out = seq_restore_on_stop(&mut world, &json!({ "value": false, "player": player.to_bits() }))
            .expect("agent");
        assert_eq!(out["restore_on_stop"], false, "{out}");
        assert_eq!(out["changed"], true);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
        let mut panel = world.remove_resource::<SeqCurvePanel>().expect("panel");
        crate::seq_curve_panel::save_sequence(&mut world, &mut panel, None).expect("save");
        let text = std::fs::read_to_string(root.join(SEQ)).expect("saved");
        assert!(!Sequence::from_ron(&text).expect("parses").restore_on_stop);
        assert!(
            !text.contains("restore_on_stop"),
            "the default is left out: {text}"
        );
        assert!(
            seq_restore_on_stop(&mut world, &json!({})).is_err(),
            "value is required"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (2): a play and stop of the saved file restores the bound property only when
    /// the flag is set (the player a play session makes from the file).
    #[test]
    fn a_play_and_stop_restores_only_when_set() {
        for on in [true, false] {
            let mut sequence = Sequence::from_ron(TEXT).expect("parses");
            sequence.restore_on_stop = on;
            let text = sequence.to_ron().expect("writes");
            let sequence = Sequence::from_ron(&text).expect("reads");
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, SequencerPlugin))
                .insert_resource(TimeUpdateStrategy::ManualDuration(
                    std::time::Duration::from_millis(100),
                ));
            let start = regia_core::math::Vec3::new(-2.0, 1.0, 3.0);
            let door = app.world_mut().spawn(Transform::from_translation(start)).id();
            let mut player = SequencePlayer::new(sequence);
            player.end = EndPolicy::Hold;
            player.play();
            let player = app
                .world_mut()
                .spawn((
                    player,
                    SequenceBindings::default().with("door", Binding::Entity(door)),
                ))
                .id();
            for _ in 0..6 {
                app.update();
            }
            let moved = app.world().get::<Transform>(door).expect("door").translation;
            assert_ne!(moved, start, "the track moved the door");
            app.world_mut()
                .get_mut::<SequencePlayer>(player)
                .expect("player")
                .stop();
            app.update();
            let after = app.world().get::<Transform>(door).expect("door").translation;
            if on {
                assert_eq!(after, start, "restored on stop");
            } else {
                assert_eq!(after, moved, "kept on stop");
            }
        }
    }
}
