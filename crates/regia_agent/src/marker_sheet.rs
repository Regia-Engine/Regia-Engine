// SPDX-License-Identifier: MPL-2.0
//! `sequencer.marker_sheet`: export and import a sequence player's markers as CSV
//! (ADR-4597, book `agents/sequence-player.md`).
//!
//! `export` returns the sheet. `import` replaces the marker list and nothing else. A bad row
//! answers `seq_marker_csv` with the row number and writes nothing. `.seq.ron` does not change
//! version. This is an agent call, not per-frame work.

use regia_app::prelude::*;
use regia_framework::sequencer::marker_sheet::{self, MarkerCsvError};
use regia_framework::sequencer::{Marker, SequencePlayer};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn error(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

fn bad(code: &str, message: impl Into<String>) -> RpcError {
    error(
        INVALID_PARAMS,
        code,
        message,
        "see book/agents/sequence-player.md (marker sheet)",
    )
}

fn csv_error(err: MarkerCsvError) -> RpcError {
    let row = err.row;
    let mut rpc = error(
        INVALID_PARAMS,
        err.code(),
        err.to_string(),
        "the marker list was not changed; columns are time_seconds, name, payload",
    );
    if let Some(Value::Object(data)) = rpc.data.as_mut() {
        data.insert("row".into(), json!(row));
    }
    rpc
}

fn entity_field(params: &Value, field: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(field).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| bad("bad_entity", format!("`{field}` must be an entity id"))),
    }
}

fn players(world: &mut World) -> Vec<Entity> {
    // A query of a component this world has never seen panics. Registering is idempotent.
    let _ = world.register_component::<SequencePlayer>();
    let mut query = world.query_filtered::<Entity, With<SequencePlayer>>();
    let mut list: Vec<Entity> = query.iter(world).collect();
    list.sort_unstable_by_key(|entity| entity.to_bits());
    list
}

fn named_player(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    // `get` of a component this world has never seen panics. Registering is idempotent.
    let _ = world.register_component::<SequencePlayer>();
    if let Some(entity) = entity_field(params, "player")? {
        return if world.get::<SequencePlayer>(entity).is_some() {
            Ok(entity)
        } else {
            Err(error(
                NOT_FOUND,
                "unknown_player",
                format!("entity {} has no sequence player", entity.to_bits()),
                "list players with sequence.player { \"action\": \"query\" }",
            ))
        };
    }
    match players(world).as_slice() {
        [] => Err(error(
            NOT_FOUND,
            "no_player",
            "no sequence player exists",
            "sequence.player { \"action\": \"load\", \"seq_ron\": \"...\" } creates one",
        )),
        [only] => Ok(*only),
        many => Err(bad(
            "several_players",
            format!("{} players exist; pass `player`", many.len()),
        )),
    }
}

fn marker_json(markers: &[Marker]) -> Vec<Value> {
    markers
        .iter()
        .map(|marker| {
            json!({
                "time_seconds": marker.time,
                "name": marker.name,
                "payload": marker.payload,
            })
        })
        .collect()
}

/// `sequencer.marker_sheet { op, player?, csv? }`.
///
/// `op` is `export` or `import`. Import replaces the player's marker list when the CSV is
/// valid and leaves every channel as it was.
pub(crate) fn marker_sheet(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = match params.get("op") {
        Some(Value::String(op)) if op == "export" || op == "import" => op.as_str(),
        _ => return Err(bad("bad_setting", "`op` must be \"export\" or \"import\"")),
    };
    let entity = named_player(world, params)?;
    if op == "export" {
        return export(world, entity);
    }
    import(world, entity, params)
}

fn export(world: &World, entity: Entity) -> Result<Value, RpcError> {
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let markers = &player.sequence().markers;
    let csv = marker_sheet::markers_to_csv(markers);
    let count = markers.len();
    let listed = marker_json(markers);
    Ok(json!({
        "player": entity.to_bits(),
        "op": "export",
        "count": count,
        "csv": csv,
        "markers": listed,
    }))
}

fn import(world: &mut World, entity: Entity, params: &Value) -> Result<Value, RpcError> {
    let csv = match params.get("csv") {
        Some(Value::String(csv)) => csv.as_str(),
        Some(_) => return Err(bad("bad_setting", "`csv` must be a string")),
        None => return Err(bad("bad_setting", "`csv` is required for import")),
    };
    let mut sequence = {
        let Some(player) = world.get::<SequencePlayer>(entity) else {
            return Err(bad("unknown_player", "the player is gone"));
        };
        player.sequence().clone()
    };
    marker_sheet::apply_marker_csv(&mut sequence, csv).map_err(csv_error)?;
    let count = sequence.markers.len();
    let csv = marker_sheet::markers_to_csv(&sequence.markers);
    let listed = marker_json(&sequence.markers);
    let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    player.replace_sequence(sequence);
    Ok(json!({
        "player": entity.to_bits(),
        "op": "import",
        "count": count,
        "csv": csv,
        "markers": listed,
    }))
}

#[cfg(test)]
mod tests {
    use regia_framework::sequencer::{
        AudioSection, BoolKey, Channel, Ease, ScalarKey, Sequence, SequencePlayer, Track,
    };
    use serde_json::json;

    use super::*;
    use crate::protocol::RpcError;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn row(err: &RpcError) -> Option<u64> {
        err.data.as_ref().and_then(|data| data["row"].as_u64())
    }

    fn sample() -> Sequence {
        Sequence {
            name: "intro".into(),
            tracks: vec![
                Track {
                    name: "fade".into(),
                    target: "ui".into(),
                    property: "opacity".into(),
                    keys: Channel::Float(vec![
                        ScalarKey::new(0.0, 0.0, Ease::Linear),
                        ScalarKey::new(2.0, 1.0, Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "bell".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Audio(vec![AudioSection::new("bell", 0.5, 1.5)]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "show".into(),
                    target: "ui".into(),
                    property: "visible".into(),
                    keys: Channel::Bool(vec![BoolKey {
                        time: 0.0,
                        value: true,
                    }]),
                    enabled: true,
                    locked: false,
                },
            ],
            markers: vec![
                regia_framework::sequencer::Marker {
                    name: "late".into(),
                    time: 2.0,
                    payload: "z".into(),
                },
                regia_framework::sequencer::Marker {
                    name: "early".into(),
                    time: 0.5,
                    payload: String::new(),
                },
                regia_framework::sequencer::Marker {
                    name: "mid".into(),
                    time: 1.0,
                    payload: "a,b".into(),
                },
            ],
            ..Sequence::default()
        }
    }

    fn spawn(app: &mut App, sequence: Sequence) -> Entity {
        app.world_mut().spawn(SequencePlayer::new(sequence)).id()
    }

    /// Tracks, marker names and saved text of the only player.
    fn snapshot(app: &mut App) -> (Vec<Track>, Vec<String>, String) {
        let mut query = app.world_mut().query::<&SequencePlayer>();
        let sequence = query.single(app.world()).expect("one player").sequence().clone();
        let tracks = sequence.tracks.clone();
        let names = sequence
            .markers
            .iter()
            .map(|marker| marker.name.clone())
            .collect();
        let ron = sequence.to_ron().expect("saves");
        (tracks, names, ron)
    }

    #[test]
    fn sequencer_marker_sheet_round_trips_and_refuses_bad_rows() {
        let method = "sequencer.marker_sheet";
        assert_eq!(method, "sequencer.marker_sheet");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        assert_eq!(
            code(&marker_sheet(app.world_mut(), &json!({ "op": "export" })).expect_err("none")),
            "no_player"
        );

        let sequence = sample();
        let tracks = sequence.tracks.clone();
        let before = sequence.to_ron().expect("saves");
        let hash = blake3::hash(before.as_bytes());
        spawn(&mut app, sequence);

        let exported = marker_sheet(app.world_mut(), &json!({ "op": "export" })).expect("export");
        let csv = exported["csv"].as_str().expect("csv").to_string();
        let names: Vec<&str> = exported["markers"]
            .as_array()
            .expect("markers")
            .iter()
            .map(|marker| marker["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["late", "early", "mid"]);

        let imported = marker_sheet(app.world_mut(), &json!({ "op": "import", "csv": csv }))
            .expect("import of the exported sheet");
        assert_eq!(imported["count"], 3);
        let (_, _, saved) = snapshot(&mut app);
        assert_eq!(blake3::hash(saved.as_bytes()), hash, "{saved}");

        let replaced = "time_seconds,name,payload\n0,only,\n1.5,later,cue\n";
        marker_sheet(app.world_mut(), &json!({ "op": "import", "csv": replaced })).expect("replace");
        let (got_tracks, names, _) = snapshot(&mut app);
        assert_eq!(got_tracks, tracks, "import changed a channel");
        assert_eq!(names, ["only", "later"]);

        let duplicate = "time_seconds,name,payload\n0,only,\n1,only,\n";
        let err = marker_sheet(app.world_mut(), &json!({ "op": "import", "csv": duplicate }))
            .expect_err("duplicate");
        assert_eq!(code(&err), "seq_marker_csv");
        assert_eq!(row(&err), Some(3));

        let negative = "time_seconds,name,payload\n-1,nope,\n";
        let err =
            marker_sheet(app.world_mut(), &json!({ "op": "import", "csv": negative })).expect_err("negative");
        assert_eq!(code(&err), "seq_marker_csv");
        assert_eq!(row(&err), Some(2));

        let unknown = "time_seconds,name,note\n0,a,\n";
        let err =
            marker_sheet(app.world_mut(), &json!({ "op": "import", "csv": unknown })).expect_err("column");
        assert_eq!(code(&err), "seq_marker_csv");
        assert_eq!(row(&err), Some(1));
        assert!(err.message.contains("unknown column"), "{err:?}");

        let (got_tracks, names, _) = snapshot(&mut app);
        assert_eq!(got_tracks, tracks);
        assert_eq!(names.len(), 2, "a refused import wrote markers");

        assert_eq!(
            code(&marker_sheet(app.world_mut(), &json!({})).expect_err("op")),
            "bad_setting"
        );
        assert_eq!(
            code(
                &marker_sheet(app.world_mut(), &json!({ "op": "export", "player": "nope" })).expect_err("id")
            ),
            "bad_entity"
        );
    }

    #[test]
    fn a_player_id_with_no_sequence_player_is_unknown() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let id = app.world_mut().spawn_empty().id();
        let err = marker_sheet(
            app.world_mut(),
            &json!({ "op": "export", "player": id.to_bits() }),
        )
        .expect_err("not a player");
        assert_eq!(code(&err), "unknown_player");
    }

    #[test]
    fn sequencer_marker_sheet_refuses_two_players_without_an_id() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        spawn(&mut app, sample());
        spawn(&mut app, sample());
        let err = marker_sheet(app.world_mut(), &json!({ "op": "export" })).expect_err("two");
        assert_eq!(code(&err), "several_players");
    }
}
