// SPDX-License-Identifier: MPL-2.0
#![cfg(test)]

use regia_agent::extension::AgentExtension;

#[test]
fn collab_agent_surface_is_registered() {
    let mut world = World::new();
    crate::agent_ext::install(&mut world);
    let extension = world.resource::<AgentExtension>();
    for method in [
        "editor.collab_host",
        "editor.collab_join",
        "editor.collab_leave",
        "editor.collab_participants",
        "editor.collab_jump",
    ] {
        assert!(extension.method(method).is_some(), "missing {method}");
    }
}

// The base lacks this module/surface: the initial registration check is the first
// acceptance failure. The tests below exercise the implemented presence contract.

use super::*;
use crate::selection::SelectMode;

fn pair() -> (World, World, transport::Loopback) {
    let hub = transport::Loopback::default();
    let mut host = World::new();
    let mut guest = World::new();
    host.insert_resource(Session::host(hub.bind(1), "Host".into()));
    guest.insert_resource(Session::join(hub.bind(2), hub.address(1), "Guest".into()));
    (host, guest, hub)
}

fn step(world: &mut World, n: u64) {
    tick_at(world, Duration::from_millis(n * 100));
}

fn accept(host: &mut World, id: u64) {
    agent::host(host, &serde_json::json!({"action":"accept", "participant":id})).expect("accept");
}

fn admit(host: &mut World, guest: &mut World) {
    step(guest, 0);
    step(host, 0);
    assert!(host.resource::<Session>().participants().len() == 1);
    accept(host, 2);
    step(host, 1);
    step(guest, 1);
    step(host, 2);
    step(guest, 2);
}

#[test]
fn collab_two_worlds_converge_in_three_ticks_and_jump_preserves_selection() {
    let (mut host, mut guest, _) = pair();
    // Different entity handles, same saved scene identity.
    let guid = EntityGuid(Guid::from_u128(42));
    let h = host.spawn(guid).id();
    guest.spawn_empty();
    let g = guest.spawn(guid).id();
    assert_ne!(h, g);
    let mut hs = Selection::default();
    hs.select(h, SelectMode::Replace);
    host.insert_resource(hs);
    let mut gs = Selection::default();
    gs.select(g, SelectMode::Replace);
    guest.insert_resource(gs);
    let camera = EditorCamera {
        yaw: 1.2,
        pitch: 0.3,
        distance: 12.0,
        ..Default::default()
    };
    host.insert_resource(camera.clone());
    let guest_camera = EditorCamera {
        yaw: -0.8,
        speed: 10.0,
        ..Default::default()
    };
    guest.insert_resource(guest_camera.clone());
    admit(&mut host, &mut guest);
    assert_eq!(host.resource::<Session>().participants().len(), 2);
    let remote = guest.resource::<Session>().remote_participants();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].presence.selection, [guid.0]);
    assert_eq!(remote[0].presence.camera.yaw, camera.yaw);
    assert_eq!(selected_by(&guest, g).len(), 1);
    assert_eq!(selected_by(&host, h).len(), 1);
    agent::jump(&mut guest, &serde_json::json!({"participant":1})).expect("jump");
    assert_eq!(
        host.resource::<Session>().remote_participants()[0]
            .presence
            .camera
            .yaw,
        guest_camera.yaw
    );
    assert_eq!(
        guest.resource::<EditorCamera>(),
        &EditorCamera {
            speed: guest_camera.speed,
            ..camera
        }
    );
    assert!(guest.resource::<Selection>().contains(g));
    agent::leave(&mut guest, &serde_json::json!({})).expect("leave");
    assert!(selected_by(&guest, g).is_empty());
    step(&mut host, 3);
    assert!(selected_by(&host, h).is_empty());
    assert_eq!(host.resource::<Session>().participants().len(), 1);
}

#[test]
fn collab_third_join_has_no_presence_until_explicit_acceptance() {
    let (mut host, mut guest, hub) = pair();
    admit(&mut host, &mut guest);
    let mut third = World::new();
    third.insert_resource(Session::join(hub.bind(3), hub.address(1), "Third".into()));
    step(&mut third, 3);
    step(&mut host, 3);
    step(&mut third, 4);
    step(&mut guest, 4);
    assert!(third.resource::<Session>().participants().is_empty());
    assert_eq!(guest.resource::<Session>().participants().len(), 2);
    assert_eq!(host.resource::<Session>().pending().len(), 1);
    assert!(agent::jump(&mut host, &serde_json::json!({"participant":3})).is_err());
    accept(&mut host, 3);
    step(&mut host, 4);
    step(&mut third, 5);
    assert_eq!(third.resource::<Session>().participants().len(), 3);
}

#[test]
fn collab_rate_is_bounded_and_silence_clears_indicators() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    let sent = host.resource::<Session>().sent;
    for ms in 201..300 {
        tick_at(&mut host, Duration::from_millis(ms));
    }
    assert_eq!(host.resource::<Session>().sent, sent);
    step(&mut guest, 60);
    assert!(guest.resource::<Session>().participants().is_empty());
    assert_eq!(
        guest.resource::<Session>().error.as_deref(),
        Some("collab_timeout")
    );
    // The host reads the guest's last queued state while it is fresh, then hears five
    // seconds of silence. (Jumping straight from 0.2 s to 6 s would be a host hitch: the
    // queued state is read first and counts as fresh, see the hitch test below.)
    step(&mut host, 3);
    step(&mut host, 60);
    assert_eq!(host.resource::<Session>().participants().len(), 1);
}

/// Review finding 2: a host hitch must read queued datagrams before judging silence.
#[test]
fn collab_host_hitch_reads_queued_presence_before_expiring_peers() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    // The host stalls after 0.2 s; the guest keeps publishing.
    for n in 3..=49 {
        step(&mut guest, n);
    }
    step(&mut host, 52);
    assert_eq!(
        host.resource::<Session>().participants().len(),
        2,
        "a live guest was expired before its queued presence was read"
    );
    step(&mut guest, 53);
    assert_eq!(guest.resource::<Session>().participants().len(), 2);
    assert_eq!(guest.resource::<Session>().error, None);
}

/// Review finding 2, client side: a joined editor's hitch must not close a live session.
#[test]
fn collab_client_hitch_reads_queued_snapshot_before_timing_out() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    // The guest stalls after 0.2 s; the host keeps publishing snapshots.
    for n in 3..=49 {
        step(&mut host, n);
    }
    step(&mut guest, 52);
    assert_eq!(
        guest.resource::<Session>().error.as_deref(),
        None,
        "the client timed out with a fresh snapshot queued"
    );
    assert_eq!(guest.resource::<Session>().participants().len(), 2);
    step(&mut host, 52);
    assert_eq!(host.resource::<Session>().participants().len(), 2);
}

/// A replayed (not newer) state never refreshes an accepted peer, so silence still expires it.
#[test]
fn collab_stale_sequence_does_not_keep_a_silent_peer_alive() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    step(&mut host, 3);
    let (from, nonce, epoch) = {
        let session = guest.resource::<Session>();
        let Role::Client(client) = &session.role else {
            panic!("client")
        };
        (
            session
                .transport
                .as_ref()
                .expect("transport")
                .address()
                .expect("address"),
            client.nonce,
            client.epoch.expect("epoch"),
        )
    };
    {
        let mut session = host.resource_mut::<Session>();
        let Role::Host(h) = &session.role else {
            panic!("host")
        };
        let seq = h.peers[&from].sequence;
        session.handle(
            from,
            Message::State {
                nonce,
                epoch,
                seq,
                presence: Presence::default(),
            },
            Duration::from_millis(5_000),
        );
    }
    step(&mut host, 54);
    assert_eq!(host.resource::<Session>().participants().len(), 1);
}

/// Review finding 3: a rejected joiner may retry from the same socket with a new request.
#[test]
fn collab_rejected_retry_from_same_socket_becomes_pending_again() {
    let (mut host, mut guest, hub) = pair();
    step(&mut guest, 0);
    step(&mut host, 0);
    agent::host(&mut host, &serde_json::json!({"action":"reject","participant":2})).expect("reject");
    step(&mut host, 1);
    step(&mut guest, 1);
    assert!(matches!(guest.resource::<Session>().role, Role::Off));
    // Same local address (a fixed port), new join nonce.
    guest.insert_resource(Session::join(hub.bind(2), hub.address(1), "Guest".into()));
    step(&mut guest, 2);
    step(&mut host, 2);
    step(&mut guest, 3);
    let pending = host.resource::<Session>().pending();
    assert_eq!(pending.len(), 1, "the retry never became pending");
    assert_eq!(pending[0]["id"], 3);
    assert!(matches!(guest.resource::<Session>().role, Role::Client(_)));
    assert_eq!(guest.resource::<Session>().error, None);
    // The new request still needs explicit acceptance; then it joins.
    assert!(guest.resource::<Session>().participants().is_empty());
    accept(&mut host, 3);
    step(&mut host, 3);
    step(&mut guest, 4);
    assert_eq!(guest.resource::<Session>().participants().len(), 2);
}

/// Review finding 3: a pending joiner that restarts (its leave was lost) replaces its request.
#[test]
fn collab_pending_restart_from_same_socket_replaces_the_request() {
    let (mut host, mut guest, hub) = pair();
    step(&mut guest, 0);
    step(&mut host, 0);
    assert_eq!(host.resource::<Session>().pending()[0]["id"], 2);
    guest.insert_resource(Session::join(hub.bind(2), hub.address(1), "Guest 2".into()));
    step(&mut guest, 1);
    step(&mut host, 1);
    let pending = host.resource::<Session>().pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], 3, "the restarted request was ignored");
    assert_eq!(pending[0]["name"], "Guest 2");
    assert!(agent::host(&mut host, &serde_json::json!({"action":"accept","participant":2})).is_err());
}

/// An accepted participant is never replaced by a different nonce from its address.
#[test]
fn collab_new_nonce_never_replaces_an_accepted_participant() {
    let (mut host, mut guest, hub) = pair();
    admit(&mut host, &mut guest);
    let mut intruder = World::new();
    intruder.insert_resource(Session::join(hub.bind(2), hub.address(1), "Other".into()));
    step(&mut intruder, 3);
    step(&mut host, 3);
    let session = host.resource::<Session>();
    assert!(session.pending().is_empty());
    let participants = session.participants();
    assert_eq!(participants.len(), 2);
    assert_eq!(participants[1].id, 2);
    assert_eq!(participants[1].name, "Guest");
}

#[test]
fn collab_host_leave_clears_remote_camera_and_selection() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    agent::leave(&mut host, &serde_json::json!({})).expect("leave host");
    step(&mut guest, 3);
    assert!(guest.resource::<Session>().remote_participants().is_empty());
    assert_eq!(
        guest.resource::<Session>().error.as_deref(),
        Some("collab_host_left")
    );
}

#[test]
fn collab_invalid_addresses_and_names_never_open_a_socket() {
    for address in [
        "example.com:48100",
        "8.8.8.8:48100",
        "224.0.0.1:48100",
        "127.0.0.1:0",
    ] {
        let mut world = World::new();
        assert!(agent::join(&mut world, &serde_json::json!({"address":address,"name":"Guest"})).is_err());
        assert!(!world.contains_resource::<Session>());
    }
    for name in ["", " \n", "a\u{001b}b"] {
        let mut world = World::new();
        assert!(
            agent::host(
                &mut world,
                &serde_json::json!({"name":name,"address":"127.0.0.1:0"})
            )
            .is_err()
        );
        assert!(!world.contains_resource::<Session>());
    }
}

#[test]
fn collab_panel_keyboard_path_hosts_and_leaves_without_pointer() {
    let mut world = World::new();
    world.insert_resource(ui::PanelState::default());
    world.resource_mut::<ui::PanelState>().address = "127.0.0.1:0".into();
    assert!(crate::command_palette::run(&mut world, "window.collaboration").is_some());
    assert!(world.resource::<ui::PanelState>().open);
    let ctx = egui::Context::default();
    let frame = |world: &mut World, events| {
        let mut controls = Vec::new();
        let mut output = ctx.run_ui(
            crate::canvas_keys::test_input::raw([800.0, 600.0], events),
            |ui| {
                controls = ui::panel(ui, world);
            },
        );
        output.textures_delta.clear();
        controls
    };
    let controls = frame(&mut world, vec![]);
    let host = controls.iter().find(|(name, _)| name == "Host").expect("Host").1;
    for _ in 0..10 {
        frame(
            &mut world,
            vec![crate::canvas_keys::test_input::key(
                egui::Key::Tab,
                egui::Modifiers::NONE,
            )],
        );
        if ctx.memory(|m| m.focused()) == Some(host) {
            break;
        }
    }
    assert_eq!(ctx.memory(|m| m.focused()), Some(host));
    frame(
        &mut world,
        vec![crate::canvas_keys::test_input::key(
            egui::Key::Enter,
            egui::Modifiers::NONE,
        )],
    );
    assert!(matches!(world.resource::<Session>().role, Role::Host(_)));
    let controls = frame(&mut world, vec![]);
    let leave = controls
        .iter()
        .find(|(name, _)| name == "Leave")
        .expect("Leave")
        .1;
    for _ in 0..10 {
        frame(
            &mut world,
            vec![crate::canvas_keys::test_input::key(
                egui::Key::Tab,
                egui::Modifiers::NONE,
            )],
        );
        if ctx.memory(|m| m.focused()) == Some(leave) {
            break;
        }
    }
    assert_eq!(ctx.memory(|m| m.focused()), Some(leave));
    frame(
        &mut world,
        vec![crate::canvas_keys::test_input::key(
            egui::Key::Enter,
            egui::Modifiers::NONE,
        )],
    );
    assert!(matches!(world.resource::<Session>().role, Role::Off));
}

#[test]
fn collab_stale_wrong_epoch_and_unaccepted_packets_cannot_change_presence() {
    let (mut host, mut guest, _) = pair();
    admit(&mut host, &mut guest);
    let (from, nonce, epoch) = {
        let session = guest.resource::<Session>();
        let Role::Client(client) = &session.role else {
            panic!("client")
        };
        (
            session
                .transport
                .as_ref()
                .expect("transport")
                .address()
                .expect("address"),
            client.nonce,
            client.epoch.expect("epoch"),
        )
    };
    let mut session = host.resource_mut::<Session>();
    let original = session
        .participants()
        .into_iter()
        .find(|p| p.id == 2)
        .expect("guest")
        .presence;
    let mut changed = original.clone();
    changed.camera.yaw += 1.0;
    for (token, session_epoch, seq) in [
        (nonce, epoch, 1),
        (nonce, Guid::from_u128(3), 99),
        (Guid::from_u128(4), epoch, 99),
    ] {
        session.handle(
            from,
            Message::State {
                nonce: token,
                epoch: session_epoch,
                seq,
                presence: changed.clone(),
            },
            Duration::from_millis(300),
        );
    }
    assert_eq!(
        session
            .participants()
            .into_iter()
            .find(|p| p.id == 2)
            .expect("guest")
            .presence,
        original
    );
    session.handle(
        from,
        Message::State {
            nonce,
            epoch,
            seq: 100,
            presence: changed.clone(),
        },
        Duration::from_millis(300),
    );
    assert_eq!(
        session
            .participants()
            .into_iter()
            .find(|p| p.id == 2)
            .expect("guest")
            .presence,
        changed
    );
}

#[test]
fn collab_limits_bound_names_selection_roster_and_input_work() {
    assert!(!valid_name(&"x".repeat(65)));
    let mut presence = Presence {
        selection: vec![Guid::from_u128(1); MAX_SELECTION + 1],
        ..Default::default()
    };
    assert!(!presence.valid());
    presence.selection.clear();
    presence.camera.distance = f32::NAN;
    assert!(!presence.valid());
    let (mut host, mut guest, hub) = pair();
    admit(&mut host, &mut guest);
    let mut roster = host.resource::<Session>().participants();
    assert!(valid_roster(&roster, 2));
    roster[1].id = 1;
    assert!(!valid_roster(&roster, 2));
    let sender = hub.bind(99);
    for _ in 0..64 {
        sender.send(b"invalid JSON", hub.address(1)).expect("send");
    }
    let received = host.resource::<Session>().received;
    step(&mut host, 3);
    assert_eq!(host.resource::<Session>().received - received, MAX_RECEIVE as u64);
    // Sending after a large time jump never catches up with a burst.
    let before = host.resource::<Session>().sent;
    step(&mut host, 40);
    assert_eq!(host.resource::<Session>().sent - before, 1);
}

#[test]
fn collab_rejection_and_reconnect_require_fresh_acceptance() {
    let (mut host, mut guest, hub) = pair();
    step(&mut guest, 0);
    step(&mut host, 0);
    agent::host(&mut host, &serde_json::json!({"action":"reject","participant":2})).expect("reject");
    step(&mut host, 1);
    step(&mut guest, 1);
    assert!(matches!(guest.resource::<Session>().role, Role::Off));
    assert_eq!(
        guest.resource::<Session>().error.as_deref(),
        Some("collab_rejected")
    );
    guest.insert_resource(Session::join(hub.bind(3), hub.address(1), "Guest".into()));
    step(&mut guest, 2);
    step(&mut host, 2);
    step(&mut guest, 3);
    assert!(guest.resource::<Session>().participants().is_empty());
    assert_eq!(host.resource::<Session>().pending().len(), 1);
}

#[test]
fn collab_presence_overlay_uses_theme_and_leave_restores_control_image() {
    use crate::picking::IdBuffer;
    use crate::render::{PaneFrame, PaneInfo};
    use crate::views::ViewCamera;
    let (mut host, mut guest, _) = pair();
    let guid = EntityGuid(Guid::from_u128(49));
    let host_entity = host.spawn(guid).id();
    let guest_entity = guest.spawn(guid).id();
    let mut selection = Selection::default();
    selection.select(host_entity, SelectMode::Replace);
    host.insert_resource(selection);
    admit(&mut host, &mut guest);
    let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
    let image = egui::ColorImage::filled([9, 9], crate::theme::tokens(&guest).panel);
    let mut ids = vec![0; 81];
    ids[40] = 1;
    let mut frame = PaneFrame {
        image: image.clone(),
        view,
        ids: Some(IdBuffer {
            width: 9,
            height: 9,
            ids,
            distance: vec![1.0; 81],
            entities: vec![guest_entity],
        }),
        info: PaneInfo::default(),
    };
    indicators::paint(&mut frame, &guest);
    assert_ne!(
        frame.image, image,
        "remote selection draws without a local selection"
    );
    assert!(
        frame
            .image
            .pixels
            .contains(&crate::theme::tokens(&guest).overlay.icon_camera)
    );
    agent::leave(&mut guest, &serde_json::json!({})).expect("leave");
    frame.image = image.clone();
    indicators::paint(&mut frame, &guest);
    assert_eq!(frame.image, image);
}

#[test]
fn collab_host_capacity_is_an_atomic_refusal_and_pending_never_publishes() {
    let (mut host, _, hub) = pair();
    let mut guests = Vec::new();
    for id in 2..=9 {
        let mut guest = World::new();
        guest.insert_resource(Session::join(hub.bind(id), hub.address(1), format!("Guest {id}")));
        step(&mut guest, u64::from(id));
        step(&mut host, u64::from(id));
        if id < 9 {
            accept(&mut host, u64::from(id));
        }
        guests.push(guest);
    }
    assert_eq!(host.resource::<Session>().participants().len(), MAX_PARTICIPANTS);
    let error =
        agent::host(&mut host, &serde_json::json!({"action":"accept","participant":9})).expect_err("full");
    assert_eq!(error.data.as_ref().expect("data")["code"], "collab_full");
    assert_eq!(host.resource::<Session>().pending().len(), 1);
    let snapshot = host.resource::<Session>().participants();
    assert!(snapshot.iter().all(|p| p.id != 9));
    assert!(valid_roster(&snapshot, 2));
}

#[test]
fn collab_schema_contracts_and_guide_cover_every_method() {
    for (schema, _) in methods() {
        let contract = regia_agent::contract::contract(schema.name).expect("contract");
        assert!(regia_agent::guide_mentions(schema.name));
        for key in contract.example_json().as_object().expect("object").keys() {
            assert!(schema.params.iter().any(|param| param.name == key));
        }
        let mut errors = contract.errors.to_vec();
        errors.sort_unstable();
        errors.dedup();
        assert_eq!(errors, contract.errors);
        for error in errors {
            assert!(include_str!("../../../../book/agents/collaboration.md").contains(error));
        }
    }
}

struct Dispatcher {
    app: regia_app::App,
    inbox: crossbeam_channel::Sender<regia_agent::server::Inbound>,
    project: std::path::PathBuf,
}

impl Dispatcher {
    fn new() -> Self {
        use regia_app::prelude::*;
        let project = regia_test_utils::unique_temp_dir("collab-dispatch");
        std::fs::create_dir_all(&project).expect("project");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, receiver) = crossbeam_channel::unbounded();
        app.world_mut().insert_resource(regia_agent::AgentInbox(receiver));
        Self { app, inbox, project }
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
        use regia_agent::protocol::Request;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(regia_agent::server::Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("send");
        self.app.update();
        let response = answer.try_recv().expect("one frame answer");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), _) => Ok(result),
            _ => panic!("empty response"),
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

#[test]
fn collab_dispatcher_two_editors_real_loopback_admission_jump_leave_and_permissions() {
    use serde_json::json;
    let mut host = Dispatcher::new();
    let mut guest = Dispatcher::new();
    let status = host.call("editor.collab_participants", json!({})).expect("off");
    assert_eq!(status["state"], "off");
    let hosted = host
        .call(
            "editor.collab_host",
            json!({"address":"127.0.0.1:0","name":"Host"}),
        )
        .expect("host");
    guest
        .call(
            "editor.collab_join",
            json!({"address":hosted["address"],"name":"Guest"}),
        )
        .expect("join");
    step(guest.app.world_mut(), 0);
    step(host.app.world_mut(), 0);
    let status = host
        .call("editor.collab_participants", json!({}))
        .expect("pending");
    assert_eq!(status["pending"].as_array().expect("array").len(), 1);
    host.call("editor.collab_host", json!({"action":"accept","participant":2}))
        .expect("accept");
    step(host.app.world_mut(), 1);
    step(guest.app.world_mut(), 1);
    step(host.app.world_mut(), 2);
    step(guest.app.world_mut(), 2);
    assert_eq!(
        guest
            .call("editor.collab_participants", json!({}))
            .expect("joined")["state"],
        "joined"
    );
    guest
        .call("editor.collab_jump", json!({"participant":1}))
        .expect("jump");
    guest.call("editor.collab_leave", json!({})).expect("leave");
    step(host.app.world_mut(), 3);
    assert_eq!(
        host.call("editor.collab_participants", json!({})).expect("host")["participants"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
    // A read-only caller cannot start a listener, join, leave or jump.
    let mut permissions = regia_agent::AgentPermissions::default();
    permissions.config.permissions = ["read".into()].into_iter().collect();
    guest.app.world_mut().insert_resource(permissions);
    assert!(
        guest
            .call(
                "editor.collab_host",
                json!({"address":"127.0.0.1:0","name":"Denied"})
            )
            .is_err()
    );
    assert_eq!(
        guest
            .call("editor.collab_participants", json!({}))
            .expect("read allowed")["state"],
        "off"
    );
    host.call("editor.collab_leave", json!({})).expect("host leave");
}

#[test]
fn collab_camera_gizmo_changes_pixels_without_any_selection() {
    use crate::render::{PaneFrame, PaneInfo};
    use crate::views::ViewCamera;
    let (mut host, mut guest, _) = pair();
    host.insert_resource(EditorCamera {
        target: regia_core::math::Vec3::ZERO,
        distance: 1.0,
        yaw: 0.0,
        pitch: 0.0,
        ..Default::default()
    });
    admit(&mut host, &mut guest);
    let camera = EditorCamera {
        target: regia_core::math::Vec3::ZERO,
        distance: 8.0,
        yaw: 1.0,
        pitch: 0.4,
        ..Default::default()
    };
    let image = egui::ColorImage::filled([100, 100], crate::theme::tokens(&guest).panel);
    let mut frame = PaneFrame {
        image: image.clone(),
        view: ViewCamera::of(ViewKind::Perspective, &camera),
        ids: None,
        info: PaneInfo::default(),
    };
    indicators::paint(&mut frame, &guest);
    assert_ne!(frame.image, image);
    agent::leave(&mut host, &serde_json::json!({})).expect("leave");
    step(&mut guest, 3);
    frame.image = image.clone();
    indicators::paint(&mut frame, &guest);
    assert_eq!(frame.image, image);
}

#[test]
fn collab_keyboard_can_accept_reject_and_jump() {
    let (mut world, mut guest, hub) = pair();
    step(&mut guest, 0);
    step(&mut world, 0);
    let mut third = World::new();
    third.insert_resource(Session::join(hub.bind(3), hub.address(1), "Third".into()));
    step(&mut third, 1);
    step(&mut world, 1);
    let ctx = egui::Context::default();
    let frame = |world: &mut World, events| {
        let mut controls = Vec::new();
        let mut output = ctx.run_ui(
            crate::canvas_keys::test_input::raw([800.0, 600.0], events),
            |ui| {
                controls = ui::panel(ui, world);
            },
        );
        output.textures_delta.clear();
        controls
    };
    for label in ["Accept 2", "Reject 3", "Jump to 2"] {
        let controls = frame(&mut world, vec![]);
        let id = controls.iter().find(|(name, _)| name == label).expect(label).1;
        for _ in 0..20 {
            frame(
                &mut world,
                vec![crate::canvas_keys::test_input::key(
                    egui::Key::Tab,
                    egui::Modifiers::NONE,
                )],
            );
            if ctx.memory(|m| m.focused()) == Some(id) {
                break;
            }
        }
        assert_eq!(ctx.memory(|m| m.focused()), Some(id), "{label} reachable");
        frame(
            &mut world,
            vec![crate::canvas_keys::test_input::key(
                egui::Key::Enter,
                egui::Modifiers::NONE,
            )],
        );
    }
    assert!(world.resource::<Session>().pending().is_empty());
    assert_eq!(world.resource::<Session>().participants().len(), 2);
    assert!(world.contains_resource::<EditorCamera>());
}

#[test]
fn collab_pending_presence_injection_does_not_bypass_admission() {
    let (mut host, mut guest, _) = pair();
    step(&mut guest, 0);
    step(&mut host, 0);
    let (from, nonce) = {
        let session = guest.resource::<Session>();
        let Role::Client(client) = &session.role else {
            panic!("client")
        };
        (
            session
                .transport
                .as_ref()
                .expect("socket")
                .address()
                .expect("address"),
            client.nonce,
        )
    };
    let mut session = host.resource_mut::<Session>();
    let Role::Host(h) = &session.role else {
        panic!("host")
    };
    let epoch = h.epoch;
    session.handle(
        from,
        Message::State {
            nonce,
            epoch,
            seq: 100,
            presence: Presence::default(),
        },
        Duration::from_millis(200),
    );
    assert_eq!(session.participants().len(), 1);
    assert_eq!(session.pending().len(), 1);
}

#[test]
fn collab_large_local_selection_is_truncated_without_changing_local_selection() {
    let mut world = World::new();
    let mut selection = Selection::default();
    for n in 1..=100 {
        let entity = world.spawn(EntityGuid(Guid::from_u128(n))).id();
        selection.select(entity, SelectMode::Add);
    }
    world.insert_resource(selection);
    let captured = Presence::capture(&world);
    assert_eq!(captured.selection.len(), MAX_SELECTION);
    assert_eq!(world.resource::<Selection>().len(), 100);
}

#[test]
fn collab_schema_params_and_error_codes_match_contract() {
    let mut world = World::new();
    for (schema, handler) in methods() {
        let error = handler(&mut world, &serde_json::json!({"unknown":true})).expect_err("unknown field");
        let code = error.data.as_ref().expect("error data")["code"]
            .as_str()
            .expect("code");
        assert!(
            regia_agent::contract::contract(schema.name)
                .expect("contract")
                .errors
                .contains(&code)
        );
    }
}

#[test]
fn collab_join_keyboard_path_works_at_increased_ui_scale() {
    let mut world = World::new();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.75);
    let frame = |world: &mut World, events| {
        let mut controls = vec![];
        let mut output = ctx.run_ui(
            crate::canvas_keys::test_input::raw([800.0, 600.0], events),
            |ui| {
                controls = ui::panel(ui, world);
            },
        );
        output.textures_delta.clear();
        controls
    };
    let controls = frame(&mut world, vec![]);
    let join = controls.iter().find(|(name, _)| name == "Join").expect("Join").1;
    for _ in 0..12 {
        frame(
            &mut world,
            vec![crate::canvas_keys::test_input::key(
                egui::Key::Tab,
                egui::Modifiers::NONE,
            )],
        );
        if ctx.memory(|m| m.focused()) == Some(join) {
            break;
        }
    }
    assert_eq!(ctx.memory(|m| m.focused()), Some(join));
    frame(
        &mut world,
        vec![crate::canvas_keys::test_input::key(
            egui::Key::Space,
            egui::Modifiers::NONE,
        )],
    );
    assert!(matches!(world.resource::<Session>().role, Role::Client(_)));
    agent::leave(&mut world, &serde_json::json!({})).expect("leave");
}

#[test]
fn collab_datagrams_cannot_dispatch_scene_edits() {
    let (mut host, _, hub) = pair();
    let sender = hub.bind(2);
    sender
        .send(
            br#"{"jsonrpc":"2.0","id":1,"method":"editor.spawn","params":{"name":"Injected"}}"#,
            hub.address(1),
        )
        .expect("packet");
    let entities = host.entities().len();
    step(&mut host, 0);
    assert_eq!(host.entities().len(), entities);
    assert_eq!(host.resource::<Session>().invalid, 1);
    assert!(host.resource::<Session>().pending().is_empty());
}

#[test]
fn collab_jump_copies_orthographic_view_kind_and_cancels_main_ease() {
    let (mut host, mut guest, _) = pair();
    let mut layout = ViewportLayout {
        quad: true,
        active: 2,
        ..Default::default()
    };
    layout.panes[2].camera.target = regia_core::math::Vec3::new(4.0, 5.0, 6.0);
    host.insert_resource(layout.clone());
    admit(&mut host, &mut guest);
    let mut eases = crate::viewport_ops::CameraEases::default();
    eases.start(
        crate::viewport_ops::MAIN_CAMERA,
        EditorCamera::default(),
        EditorCamera::default(),
    );
    guest.insert_resource(eases);
    agent::jump(&mut guest, &serde_json::json!({"participant":1})).expect("jump");
    let result = guest.resource::<ViewportLayout>();
    assert_eq!(result.panes[0].kind, ViewKind::Front);
    assert_eq!(result.panes[0].camera.target, layout.panes[2].camera.target);
    assert!(
        guest
            .resource::<crate::viewport_ops::CameraEases>()
            .moves
            .is_empty()
    );
}

#[test]
fn collab_largest_legal_roster_fits_the_packet_budget() {
    let participants: Vec<_> = (0..MAX_PARTICIPANTS)
        .map(|n| Participant {
            id: n as u64 + 1,
            name: "\"".repeat(64),
            color: n as u8,
            presence: Presence {
                selection: (1..=MAX_SELECTION)
                    .map(|id| Guid::from_u128(id as u128))
                    .collect(),
                ..Default::default()
            },
        })
        .collect();
    assert!(valid_roster(&participants, 2));
    let packet = Envelope {
        version: 1,
        message: Message::Snapshot {
            nonce: Guid::from_u128(1),
            epoch: Guid::from_u128(2),
            seq: u64::MAX,
            you: 2,
            participants,
        },
    };
    assert!(serde_json::to_vec(&packet).expect("packet").len() <= MAX_PACKET);
}

#[test]
fn collab_shared_selection_keeps_local_and_both_remote_colors_visible() {
    use crate::picking::IdBuffer;
    use crate::render::{PaneFrame, PaneInfo};
    use crate::views::ViewCamera;
    let (mut host, mut guest, hub) = pair();
    let mut third = World::new();
    third.insert_resource(Session::join(hub.bind(3), hub.address(1), "Third".into()));
    let guid = EntityGuid(Guid::from_u128(71));
    let mut guest_entity = None;
    for (index, world) in [&mut host, &mut guest, &mut third].into_iter().enumerate() {
        let entity = world.spawn(guid).id();
        let mut selection = Selection::default();
        selection.select(entity, SelectMode::Replace);
        world.insert_resource(selection);
        if index == 1 {
            guest_entity = Some(entity);
        }
    }
    admit(&mut host, &mut guest);
    step(&mut third, 3);
    step(&mut host, 3);
    accept(&mut host, 3);
    step(&mut host, 4);
    step(&mut third, 4);
    step(&mut host, 5);
    step(&mut guest, 5);
    let mut ids = vec![0; 289];
    ids[144] = 1;
    let mut frame = PaneFrame {
        image: egui::ColorImage::filled([17, 17], crate::theme::tokens(&guest).panel),
        // Keep camera markers off screen so only selection silhouettes can supply these colors.
        view: ViewCamera::of(
            ViewKind::Perspective,
            &EditorCamera {
                target: regia_core::math::Vec3::new(10_000.0, 0.0, 0.0),
                yaw: 0.0,
                pitch: 0.0,
                ..Default::default()
            },
        ),
        ids: Some(IdBuffer {
            width: 17,
            height: 17,
            ids,
            distance: vec![1.0; 289],
            entities: vec![guest_entity.expect("guest entity")],
        }),
        info: PaneInfo::default(),
    };
    crate::overlays::paint_outline(&mut frame, &guest);
    let tokens = &crate::theme::tokens(&guest).overlay;
    for color in [tokens.selection_outline, tokens.icon_camera, tokens.icon_decal] {
        assert!(
            frame.image.pixels.contains(&color),
            "shared selection color {color:?}"
        );
    }
}

#[test]
fn collab_transient_udp_errors_do_not_end_the_host_session() {
    for error in [
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::ConnectionRefused,
        std::io::ErrorKind::ConnectionReset,
    ] {
        let (mut host, mut guest, hub) = pair();
        admit(&mut host, &mut guest);
        guest.insert_resource(EditorCamera {
            yaw: 1.7,
            ..Default::default()
        });
        step(&mut guest, 3);
        hub.fail_next(1, error).expect("receive error");
        step(&mut host, 3);
        assert!(matches!(host.resource::<Session>().role, Role::Host(_)));
        assert_eq!(
            host.resource::<Session>().remote_participants()[0]
                .presence
                .camera
                .yaw,
            1.7
        );
    }
}

#[test]
fn collab_local_unidentified_selection_does_not_reset_camera_presence() {
    let mut world = World::new();
    let identified = world.spawn(EntityGuid(Guid::from_u128(93))).id();
    let unidentified = world.spawn(EntityGuid::default()).id();
    let mut selection = Selection::default();
    selection.select(identified, SelectMode::Replace);
    selection.select(unidentified, SelectMode::Add);
    world.insert_resource(selection);
    world.insert_resource(EditorCamera {
        yaw: 1.1,
        ..Default::default()
    });
    let presence = Presence::capture(&world);
    assert_eq!(presence.selection, [Guid::from_u128(93)]);
    assert_eq!(presence.camera.yaw, 1.1);
    assert_eq!(world.resource::<Selection>().len(), 2);
}
