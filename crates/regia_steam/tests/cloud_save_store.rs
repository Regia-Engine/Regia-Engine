// SPDX-License-Identifier: MPL-2.0
//! Spec 91 § 2.2: Steam Remote Storage as the framework's save store. With
//! `SaveStore::new(SteamRemoteStorage::new(&steam))` installed, the save flow's requests, quicksave
//! and listing all go to the cloud (the mock backend here) and never touch the save directory.

use regia_app::prelude::*;
use regia_framework::save_flow::{
    SaveFlowPlugin, SaveInfo, SaveOp, SaveOutcome, SaveRequest, SaveStorage, SaveStore, list_store,
};
use regia_framework::save_game::{SaveDirectory, SavePlugin, Saveable};
use regia_steam::{
    AppId, MockSteam, SteamBackend, SteamError, SteamEvent, SteamId, SteamPlugin, SteamRemoteStorage,
    SteamState,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A saved marker component.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Relic(u32);

fn app(mock: Arc<dyn SteamBackend>, dir: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        SavePlugin::default(),
        SaveFlowPlugin,
        SteamPlugin::default().with_backend(mock),
    ))
    .register_type::<Relic>()
    .insert_resource(SaveDirectory(dir.to_path_buf()));
    app.update(); // the Steam resource exists from here
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    app.insert_resource(SaveStore::new(SteamRemoteStorage::new(&steam)));
    app
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloudOp {
    Write,
    Read,
    Delete,
    Exists,
    List,
}

#[derive(Debug, Default)]
struct ScriptedBackend {
    cloud: Mutex<BTreeMap<String, Vec<u8>>>,
    failure: Mutex<Option<(CloudOp, ScriptedFailure)>>,
}

#[derive(Debug)]
enum ScriptedFailure {
    Rejected(String),
    Unavailable,
}

impl ScriptedBackend {
    fn fail_next(&self, operation: CloudOp, message: &str) {
        *self.failure.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((operation, ScriptedFailure::Rejected(message.to_owned())));
    }

    fn go_unavailable_next(&self, operation: CloudOp) {
        *self.failure.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((operation, ScriptedFailure::Unavailable));
    }

    fn failure(&self, operation: CloudOp) -> Result<(), SteamError> {
        let mut failure = self.failure.lock().unwrap_or_else(|error| error.into_inner());
        if failure.as_ref().is_some_and(|(next, _)| *next == operation)
            && let Some((_, error)) = failure.take()
        {
            return Err(match error {
                ScriptedFailure::Rejected(message) => SteamError::Rejected(message),
                ScriptedFailure::Unavailable => SteamError::Unavailable,
            });
        }
        Ok(())
    }
}

impl SteamBackend for ScriptedBackend {
    fn initialize(&self, _app_id: Option<AppId>) -> Result<SteamState, String> {
        Ok(SteamState::Ready)
    }
    fn state(&self) -> SteamState {
        SteamState::Ready
    }
    fn pump(&self) -> Vec<SteamEvent> {
        Vec::new()
    }
    fn app_id(&self) -> Option<AppId> {
        None
    }
    fn unlock(&self, _id: &str) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn clear_achievement(&self, _id: &str) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn achievement_unlocked(&self, _id: &str) -> Result<bool, SteamError> {
        Err(SteamError::Unavailable)
    }
    fn reset_all_stats(&self, _achievements_too: bool) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn add_stat_i32(&self, _name: &str, _delta: i32) -> Result<i32, SteamError> {
        Err(SteamError::Unavailable)
    }
    fn set_stat_i32(&self, _name: &str, _value: i32) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn set_stat_f32(&self, _name: &str, _value: f32) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn stat_i32(&self, _name: &str) -> Result<i32, SteamError> {
        Err(SteamError::Unavailable)
    }
    fn stat_f32(&self, _name: &str) -> Result<f32, SteamError> {
        Err(SteamError::Unavailable)
    }
    fn store_stats(&self) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn set_rich_presence(&self, _key: &str, _value: Option<&str>) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn clear_rich_presence(&self) {}
    fn is_steam_deck(&self) -> bool {
        false
    }
    fn overlay_enabled(&self) -> bool {
        false
    }
    fn ui_language(&self) -> String {
        String::new()
    }
    fn steam_id(&self) -> Option<SteamId> {
        None
    }
    fn cloud_write(&self, name: &str, data: &[u8]) -> Result<(), SteamError> {
        self.failure(CloudOp::Write)?;
        self.cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name.to_owned(), data.to_vec());
        Ok(())
    }
    fn cloud_read(&self, name: &str) -> Result<Option<Vec<u8>>, SteamError> {
        self.failure(CloudOp::Read)?;
        Ok(self
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned())
    }
    fn cloud_delete(&self, name: &str) -> Result<(), SteamError> {
        self.failure(CloudOp::Delete)?;
        self.cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(name);
        Ok(())
    }
    fn cloud_exists(&self, name: &str) -> Result<bool, SteamError> {
        self.failure(CloudOp::Exists)?;
        Ok(self
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(name))
    }
    fn cloud_list(&self) -> Result<Vec<String>, SteamError> {
        self.failure(CloudOp::List)?;
        Ok(self
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .collect())
    }
}

fn outcomes(app: &App) -> Vec<SaveOutcome> {
    let messages = app.world().resource::<Messages<SaveOutcome>>();
    messages.get_cursor().read(messages).cloned().collect()
}

#[test]
fn quicksave_and_quickload_go_through_steam_remote_storage() {
    let dir = regia_test_utils::unique_temp_dir("steam-store");
    let mock = MockSteam::shared();
    let mut app = app(mock.clone(), &dir);
    app.world_mut().spawn((Saveable, Relic(7)));
    app.world_mut().resource_mut::<SaveInfo>().level = "harbor".into();

    app.world_mut().write_message(SaveRequest::QuickSave);
    app.update();
    let saved = outcomes(&app);
    assert!(
        saved.iter().any(|o| o.op == SaveOp::Save && o.result.is_ok()),
        "{saved:?}"
    );
    let file = mock
        .cloud_file("saves/Quick.frts")
        .expect("the quick slot is a cloud file");
    assert_eq!(&file[..4], b"FRTS", "the save container, unchanged");
    assert!(
        std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) == 0,
        "nothing was written to the save directory"
    );

    let listed = list_store(app.world()).expect("list the cloud");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Quick");
    assert!(
        listed[0].header.is_some(),
        "the header parses from the cloud bytes"
    );

    // Lose the relic and the level name; the quick load restores both from the cloud.
    let relics: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<Relic>>()
        .iter(app.world())
        .collect();
    for relic in relics {
        app.world_mut().despawn(relic);
    }
    app.world_mut().resource_mut::<SaveInfo>().level.clear();
    app.world_mut().write_message(SaveRequest::QuickLoad);
    app.update();
    let world = app.world_mut();
    assert_eq!(
        world
            .query::<&Relic>()
            .iter(world)
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        [7]
    );
    assert_eq!(app.world().resource::<SaveInfo>().level, "harbor");

    app.world_mut()
        .write_message(SaveRequest::Delete { slot: "Quick".into() });
    app.update();
    assert!(
        mock.cloud_file("saves/Quick.frts").is_none(),
        "deleted in the cloud"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unavailable_cloud_answers_with_an_error_outcome() {
    let dir = regia_test_utils::unique_temp_dir("steam-store-off");
    let mut app = app(MockSteam::shared_unavailable(), &dir);
    app.world_mut()
        .write_message(SaveRequest::Save { slot: "A".into() });
    app.update();
    let failed = outcomes(&app);
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]
            .result
            .as_ref()
            .is_err_and(|e| e.contains("unavailable")),
        "{failed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unavailable_cloud_keeps_existing_slots_and_a_retry_has_one_success_outcome() {
    let dir = regia_test_utils::unique_temp_dir("steam-store-recovery");
    let mock = MockSteam::shared();
    let mut app = app(mock.clone(), &dir);

    // Seed two independent cloud slots before taking the service offline.
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    let cloud = SteamRemoteStorage::new(&steam);
    regia_steam::SaveBackend::save(&cloud, "A", b"good old value").expect("seed A");
    regia_steam::SaveBackend::save(&cloud, "B", b"untouched").expect("seed B");

    mock.set_state(regia_steam::SteamState::Unavailable);
    app.world_mut()
        .write_message(SaveRequest::Save { slot: "A".into() });
    app.update();
    let failed = outcomes(&app);
    assert_eq!(failed.len(), 1, "one failure outcome, no success event");
    assert_eq!(failed[0].op, SaveOp::Save);
    assert!(
        failed[0]
            .result
            .as_ref()
            .is_err_and(|e| e.contains("NotConnected") || e.contains("unavailable")),
        "{failed:?}"
    );
    assert_eq!(
        mock.cloud_file("saves/A.frts").as_deref(),
        Some(&b"good old value"[..])
    );
    assert_eq!(
        mock.cloud_file("saves/B.frts").as_deref(),
        Some(&b"untouched"[..])
    );

    mock.set_state(regia_steam::SteamState::Ready);
    app.world_mut()
        .write_message(SaveRequest::Save { slot: "A".into() });
    app.update();
    let retried = outcomes(&app);
    assert_eq!(retried.len(), 2, "one failed outcome and one retry outcome");
    assert!(retried[0].result.is_err());
    assert!(retried[1].result.is_ok(), "{retried:?}");
    assert_eq!(
        mock.cloud_file("saves/B.frts").as_deref(),
        Some(&b"untouched"[..])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_listing_entries_are_ignored_without_hiding_valid_slots() {
    let dir = regia_test_utils::unique_temp_dir("steam-store-list-policy");
    let mock = MockSteam::shared();
    let app = app(mock.clone(), &dir);
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    let cloud = SteamRemoteStorage::new(&steam);
    regia_steam::SaveBackend::save(&cloud, "Good", b"good").expect("seed valid slot");

    mock.sync_remote_file("saves/.frts", b"empty slot", false);
    mock.sync_remote_file("elsewhere/Bad.frts", b"wrong root", false);
    mock.sync_remote_file("saves/Broken", b"missing extension", false);
    assert_eq!(
        regia_steam::SaveBackend::slots(&cloud).expect("list slots"),
        vec!["Good"],
        "listing policy only accepts non-empty names below saves/ with .frts"
    );
    assert_eq!(mock.cloud_file("saves/Good.frts").as_deref(), Some(&b"good"[..]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scripted_backend_failures_map_correctly_and_recover_per_slot() {
    let dir = regia_test_utils::unique_temp_dir("steam-store-scripted");
    let backend = Arc::new(ScriptedBackend::default());
    let mut app = app(backend.clone(), &dir);
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    let cloud = SteamRemoteStorage::new(&steam);
    regia_steam::SaveBackend::save(&cloud, "A", b"old").expect("seed A");
    regia_steam::SaveBackend::save(&cloud, "B", b"keep B").expect("seed B");

    backend.fail_next(CloudOp::Write, "quota exceeded");
    app.world_mut()
        .write_message(SaveRequest::Save { slot: "A".into() });
    app.update();
    let first = outcomes(&app);
    assert_eq!(first.len(), 1, "the failed write gets one failure outcome");
    assert_eq!(
        first[0].result.as_ref().err().map(String::as_str),
        Some("save IO error at saves/A.frts: Steam rejected the operation: quota exceeded"),
        "the rejected remote write maps to one framework error outcome"
    );
    assert_eq!(
        backend
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get("saves/A.frts")
            .map(Vec::as_slice),
        Some(&b"old"[..])
    );
    assert_eq!(
        backend
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get("saves/B.frts")
            .map(Vec::as_slice),
        Some(&b"keep B"[..])
    );
    assert_eq!(
        regia_steam::SaveBackend::load(&cloud, "A")
            .ok()
            .flatten()
            .as_deref(),
        Some(&b"old"[..]),
        "the value rejected by the write remains readable"
    );

    backend.fail_next(CloudOp::Read, "read refused");
    assert!(
        matches!(regia_steam::SaveBackend::load(&cloud, "A"), Err(regia_steam::SaveError::Io(error)) if error.to_string().contains("read refused"))
    );
    assert_eq!(
        regia_steam::SaveBackend::load(&cloud, "A")
            .ok()
            .flatten()
            .as_deref(),
        Some(&b"old"[..])
    );

    backend.fail_next(CloudOp::Delete, "delete refused");
    assert!(
        matches!(regia_steam::SaveBackend::delete(&cloud, "A"), Err(regia_steam::SaveError::Io(error)) if error.to_string().contains("delete refused"))
    );
    assert_eq!(
        regia_steam::SaveBackend::load(&cloud, "A")
            .ok()
            .flatten()
            .as_deref(),
        Some(&b"old"[..])
    );

    backend.fail_next(CloudOp::List, "list refused");
    assert!(
        matches!(regia_steam::SaveBackend::slots(&cloud), Err(regia_steam::SaveError::Io(error)) if error.to_string().contains("list refused"))
    );
    assert_eq!(
        regia_steam::SaveBackend::slots(&cloud).ok(),
        Some(vec!["A".into(), "B".into()])
    );

    backend.go_unavailable_next(CloudOp::Write);
    assert!(matches!(
        regia_steam::SaveBackend::save(&cloud, "A", b"during outage"),
        Err(regia_steam::SaveError::Unavailable)
    ));
    assert_eq!(
        regia_steam::SaveBackend::load(&cloud, "A")
            .ok()
            .flatten()
            .as_deref(),
        Some(&b"old"[..])
    );

    backend.go_unavailable_next(CloudOp::Write);
    let unavailable = SaveStorage::write(&cloud, "A", b"during outage")
        .expect_err("the backend becomes unavailable after the listing preflight");
    assert!(matches!(
        unavailable,
        regia_framework::save_game::SaveError::Io(error, path)
        if error.kind() == std::io::ErrorKind::NotConnected
            && path.as_path() == std::path::Path::new("saves/A.frts")
    ));
    assert_eq!(
        regia_steam::SaveBackend::load(&cloud, "A")
            .ok()
            .flatten()
            .as_deref(),
        Some(&b"old"[..]),
        "the unavailable write did not mutate either slot"
    );

    backend.fail_next(CloudOp::Write, "quota exceeded");
    let mapped = SaveStorage::write(&cloud, "A", b"replacement").expect_err("quota refusal");
    assert!(
        matches!(mapped, regia_framework::save_game::SaveError::Io(error, path)
            if error.kind() == std::io::ErrorKind::Other
                && path.as_path() == std::path::Path::new("saves/A.frts")
        )
    );
    assert!(
        SaveStorage::write(&cloud, "A", b"replacement").is_ok(),
        "the backend accepts a later retry"
    );

    app.world_mut()
        .write_message(SaveRequest::Save { slot: "A".into() });
    app.update();
    let final_outcomes = outcomes(&app);
    assert_eq!(
        final_outcomes.len(),
        2,
        "one failure outcome and exactly one successful retry outcome"
    );
    assert!(final_outcomes[0].result.is_err());
    assert!(final_outcomes[1].result.is_ok(), "{final_outcomes:?}");
    assert_eq!(
        backend
            .cloud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get("saves/B.frts")
            .map(Vec::as_slice),
        Some(&b"keep B"[..])
    );
    let _ = std::fs::remove_dir_all(&dir);
}
