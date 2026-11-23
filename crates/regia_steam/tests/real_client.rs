// SPDX-License-Identifier: MPL-2.0
//! Real-client smoke test (specs/24 § 4). Built only with `--features steamworks`, because only
//! that links Valve's Steamworks SDK (see the crate README for `STEAM_SDK_LOCATION`); inside, it
//! runs only when a Steam client is available and **skips otherwise — it never fails a machine
//! without Steam**. With Steam running it exercises the same backend surface the frame pump
//! touches, against Valve's public test app 480 (Spacewar) unless `STEAM_APP_ID` says otherwise.
//!
//! The two-account acceptance run (`two_account_lobby_and_steam_sockets`) is `#[ignore]`d: without
//! the owner's session it is listed as `ignored` (not run), never `ok` (ADR-4896).

#![cfg(not(target_os = "android"))]

use regia_steam::{AppId, RealBackend, SteamBackend, SteamState};

/// The Steamworks test app every developer can run against (Valve's Spacewar, id 480).
const TEST_APP_ID: AppId = AppId(480);

#[test]
fn steam_client_smoke() {
    let backend = RealBackend::offline();
    // Dev convenience: STEAM_APP_ID overrides the test app (e.g. the studio's own dev app).
    let app_id = regia_core::brand::env_var("STEAM_APP_ID")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(TEST_APP_ID, AppId);

    let state = match backend.initialize(Some(app_id)) {
        Ok(state) => state,
        Err(err) => {
            // Not a failure: Steam isn't running (or this build isn't entitled) — skip.
            eprintln!("skipped: no Steam client to talk to ({err})");
            return;
        }
    };
    assert_eq!(state, SteamState::Ready, "a successful init reports Ready");

    // The queries one frame touches; none may panic with Steam running. Values depend on the
    // signed-in account, so only exercise them.
    let _ = backend.pump();
    let _ = backend.overlay_enabled();
    let _ = backend.ui_language();
    let _ = backend.steam_id();
    let _ = backend.is_steam_deck();
    let _ = backend.app_id();

    // A second pump (callbacks may have arrived during the queries) and a graceful shutdown
    // (dropping the client) happen implicitly at the end of the test.
    let _ = backend.pump();

    lobby_round_trip(&backend);
    ownership_and_timeline(&backend, app_id);
    leaderboard_round_trip(&backend);
    workshop_round_trip(&backend);
}

/// ADR-2418: the Workshop over ISteamUGC. Lists the subscribed items (and the install folder of
/// each) always; uploads `REGIA_WORKSHOP_UPLOAD_DIR` as a new private item only when that is set,
/// and subscribes to `REGIA_WORKSHOP_ITEM` and mounts it at `mods://` only when that is set, so a
/// machine with Steam running never publishes by accident.
fn workshop_round_trip(backend: &RealBackend) {
    use regia_asset::mods_vfs::ModsVfs;
    use regia_steam::workshop::{RealWorkshop, SteamWorkshop, UploadStatus};
    use regia_steam::{WorkshopEvent, WorkshopItemId, WorkshopService, WorkshopUpload};

    let Some(client) = backend
        .raw_client()
        .and_then(|any| any.downcast::<steamworks::Client>().ok())
    else {
        panic!("a Ready backend hands out its client");
    };
    let service = std::sync::Arc::new(RealWorkshop::new(*client));
    assert_eq!(service.name(), "steam");
    for item in service.subscribed() {
        eprintln!(
            "subscribed {item}: {:?} {:?}",
            service.item_state(item),
            service.install_info(item)
        );
    }
    let mut workshop = SteamWorkshop::new(service);
    let mut mods = ModsVfs::new();
    let wait = |workshop: &mut SteamWorkshop, mods: &mut ModsVfs, done: &dyn Fn(&WorkshopEvent) -> bool| {
        for _ in 0..6000 {
            let _ = backend.pump();
            if workshop.update(Some(&mut *mods)).iter().any(done) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    };
    if let Ok(dir) = regia_core::brand::env_var("REGIA_WORKSHOP_UPLOAD_DIR") {
        let mut upload = WorkshopUpload::new("Regia Engine Workshop test", dir);
        upload.change_note = "real_client test upload".into();
        let request = workshop
            .upload(&upload)
            .expect("the upload passes its local checks");
        let finished = wait(
            &mut workshop,
            &mut mods,
            &|e| matches!(e, WorkshopEvent::Uploaded { request: r, .. } | WorkshopEvent::UploadFailed { request: r, .. } if *r == request),
        );
        assert!(finished, "the upload answered within 60 s");
        match &workshop.uploads()[&request] {
            UploadStatus::Done {
                item,
                needs_legal_agreement,
            } => eprintln!("uploaded item {item} (legal agreement needed: {needs_legal_agreement})"),
            other => panic!("upload failed: {other:?}"),
        }
    }
    if let Some(item) = regia_core::brand::env_var("REGIA_WORKSHOP_ITEM")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        let item = WorkshopItemId(item);
        workshop.subscribe(item).expect("subscribe is sent");
        let installed = wait(
            &mut workshop,
            &mut mods,
            &|e| matches!(e, WorkshopEvent::Installed { item: i, .. } if *i == item),
        );
        assert!(installed, "item {item} installed within 60 s");
        let status = &workshop.items()[&item];
        assert!(status.mounted.is_some(), "mounted at mods://: {status:?}");
        let first = mods.files().next().map(|(path, _)| path.to_owned());
        if let Some(path) = first {
            let bytes = mods.read(&path).expect("the mounted item's files are readable");
            eprintln!(
                "mods://{path}: {} bytes, set hash {}",
                bytes.len(),
                mods.content_set_hash()
            );
        }
    }
}

/// ADR-2340: the ownership queries and timeline calls go through without panicking; the running
/// app is owned by whoever launched it.
fn ownership_and_timeline(backend: &RealBackend, app_id: AppId) {
    use regia_steam::{TimelineCall, TimelineClip, TimelineEvent, TimelineMode};

    assert_eq!(
        backend.is_subscribed_app(app_id),
        Ok(true),
        "the running app is owned"
    );
    let _ = backend.is_dlc_installed(app_id);
    let _ = backend.is_app_installed(app_id);
    let _ = backend.app_install_dir(app_id);
    for call in [
        TimelineCall::Mode(TimelineMode::Playing),
        TimelineCall::State {
            description: "regia smoke".into(),
            delta: 0.0,
        },
        TimelineCall::Event(
            TimelineEvent::new("regia smoke")
                .span(-1.0, 1.0)
                .clip(TimelineClip::None),
        ),
        TimelineCall::ClearState { delta: 0.0 },
    ] {
        assert_eq!(backend.timeline(&call), Ok(()), "{call:?}");
    }
}

/// ADR-2340: find a board (`REGIA_STEAM_LEADERBOARD`, else Spacewar's public `Feet Traveled`),
/// download the top ten and the rows around the user. No upload: the smoke test must not change
/// a real board. No reply in time is a skip, not a failure.
fn leaderboard_round_trip(backend: &RealBackend) {
    use regia_framework::leaderboard::{LeaderboardRange, LeaderboardService, LeaderboardSpec, ServiceEvent};

    let Some(service) = backend.leaderboards() else {
        panic!("a Ready backend offers leaderboards");
    };
    assert_eq!(service.name(), "steam");
    let name =
        regia_core::brand::env_var("REGIA_STEAM_LEADERBOARD").unwrap_or_else(|_| "Feet Traveled".into());
    let wait = |service: &dyn LeaderboardService| -> Option<ServiceEvent> {
        for _ in 0..1000 {
            let _ = backend.pump();
            if let Some(event) = service.poll().into_iter().next() {
                return Some(event);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    };
    if let Err(err) = service.find(&LeaderboardSpec::new(name.clone())) {
        panic!("find was refused locally: {err}");
    }
    let board = match wait(service.as_ref()) {
        Some(ServiceEvent::Found {
            result: Ok(Some(info)),
            ..
        }) => info,
        other => {
            eprintln!("skipped leaderboard round trip: board `{name}` not found ({other:?})");
            return;
        }
    };
    eprintln!(
        "found `{}`: {} entries, {:?}",
        board.name, board.entry_count, board.sort
    );
    for range in [
        LeaderboardRange::top(10),
        LeaderboardRange::AroundUser { before: 2, after: 2 },
    ] {
        if let Err(err) = service.download(board.handle, range) {
            panic!("download was refused locally: {err}");
        }
        match wait(service.as_ref()) {
            Some(ServiceEvent::Downloaded { result: Ok(rows), .. }) => {
                assert!(rows.len() <= 10, "{range:?}: {} rows", rows.len());
                assert!(rows.windows(2).all(|w| w[0].rank < w[1].rank), "rank order");
                eprintln!("{range:?}: {} rows", rows.len());
            }
            other => eprintln!("{range:?}: no rows ({other:?})"),
        }
    }
}

/// Spec 89 § 11: create a private lobby, read it back, leave. The reply comes from Valve's
/// servers, so the wait is bounded by pump rounds; no reply in time is a skip, not a failure.
fn lobby_round_trip(backend: &RealBackend) {
    use regia_net::lobby::{LobbyEvent, LobbySpec, LobbyVisibility};

    let Some(lobbies) = backend.lobbies() else {
        panic!("a Ready backend offers lobbies");
    };
    let mut spec = LobbySpec {
        visibility: LobbyVisibility::Private,
        max_members: 2,
        ..LobbySpec::default()
    };
    spec.data.insert("regia_smoke".into(), "1".into());
    let request = match lobbies.create(&spec) {
        Ok(request) => request,
        Err(err) => panic!("create was refused locally: {err}"),
    };
    // A real network round trip: up to 1000 pump rounds of 10 ms each.
    let mut created = None;
    for _ in 0..1000 {
        let _ = backend.pump();
        for event in lobbies.poll() {
            if let LobbyEvent::Created { request: r, result } = event
                && r == request
            {
                created = Some(result);
            }
        }
        if created.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let lobby = match created {
        Some(Ok(lobby)) => lobby,
        Some(Err(err)) => {
            eprintln!("skipped lobby round trip: create failed on the service ({err})");
            return;
        }
        None => {
            eprintln!("skipped lobby round trip: no reply from the service");
            return;
        }
    };
    let Some(info) = lobbies.lobby(lobby) else {
        panic!("a created lobby reads back");
    };
    if let Some(me) = lobbies.local_member() {
        assert_eq!(info.owner, me, "we own the lobby we created");
    }
    assert_eq!(info.max_members, 2);
    assert_eq!(info.data.get("regia_smoke").map(String::as_str), Some("1"));
    lobbies.leave(lobby);
    let _ = backend.pump();
    assert!(lobbies.lobby(lobby).is_none(), "a left lobby is gone");
}

/// Poll rounds the two-account run waits for its peer (100 ms each, about 4 minutes per endpoint).
const TWO_ACCOUNT_WAIT_ROUNDS: u32 = 2400;

/// The two-account run of specs/24 § 4 (ADR-4896): lobby create and join, the build's content
/// check, the launch to the host's Steam id, a datagram echo over Steam sockets, leave and
/// rejoin with a second echo, and a cloud round trip on each account. The script itself is
/// `regia_steam::real_run` (tested on the mock in `real_run_harness.rs`); this test only drives
/// it against the real client.
///
/// **Ignored by default, so a machine without the owner's setup reports it as `ignored`, never
/// as `ok`.** The owner runs it with `--ignored` and `REGIA_STEAM_TWO_ACCOUNT=host|guest` as
/// `book/user-guide/steam-real-run.md` specifies; run with `--ignored` but without the setup it
/// fails with `NOT RUN (<code>)`. It never reads, prints or stores a credential. Each endpoint
/// writes its evidence log to `REGIA_STEAM_RUN_LOG` (default
/// `regia-steam-real-run-<role>.log`); `cargo run -p regia_steam --example steam_real_run_verdict`
/// judges the two logs.
#[test]
#[ignore = "owner session only: REGIA_STEAM_TWO_ACCOUNT=host|guest, book/user-guide/steam-real-run.md"]
fn two_account_lobby_and_steam_sockets() {
    use regia_steam::real_run::{EndpointStatus, LOG_ENV, RealRunEndpoint, RealRunRole, RealRunSetup};

    let setup = match RealRunSetup::from_env() {
        Ok(setup) => setup,
        Err(not_run) => panic!("{not_run}"),
    };
    let role = setup.role;
    let log_path = std::path::PathBuf::from(
        regia_core::brand::env_var(LOG_ENV)
            .unwrap_or_else(|_| format!("regia-steam-real-run-{}.log", role.name())),
    );
    let backend = RealBackend::offline();
    if let Err(err) = backend.initialize(Some(setup.app_id)) {
        panic!("NOT RUN (steam_not_ready): a running, signed-in Steam client is needed: {err}");
    }
    let mut endpoint = match RealRunEndpoint::start(setup, &backend) {
        Ok(endpoint) => endpoint,
        Err(code) => panic!("NOT RUN ({code}): the endpoint did not start"),
    };
    let mut printed = 0;
    let mut announced = false;
    let mut status = EndpointStatus::Running;
    for _ in 0..TWO_ACCOUNT_WAIT_ROUNDS {
        let _ = backend.pump();
        status = endpoint.poll(&backend);
        if role == RealRunRole::Host
            && !announced
            && let Some(lobby) = endpoint.lobby()
        {
            println!("REGIA_STEAM_LOBBY_ID={}", lobby.0);
            announced = true;
        }
        for line in &endpoint.log().lines()[printed..] {
            println!("REGIA_RUN {line}");
        }
        printed = endpoint.log().lines().len();
        if status != EndpointStatus::Running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if status == EndpointStatus::Running {
        endpoint.time_out();
        for line in &endpoint.log().lines()[printed..] {
            println!("REGIA_RUN {line}");
        }
    }
    match endpoint.log().write_to(&log_path) {
        Ok(()) => println!("REGIA_RUN_LOG={}", log_path.display()),
        Err(err) => eprintln!("could not write {}: {err}", log_path.display()),
    }
    assert_eq!(
        endpoint.status(),
        &EndpointStatus::Complete,
        "the {} endpoint did not complete; its log is {}",
        role.name(),
        log_path.display()
    );
}
