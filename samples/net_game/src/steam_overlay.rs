// SPDX-License-Identifier: MIT-0
//! The client's Steam readout (feature `steam`, ADR-7317): the windowed client runs
//! `regia::steam::SteamPlugin` and shows the overlay state on its HUD, so the owner's overlay check
//! (`book/user-guide/steam-real-run.md` § 3) needs no agent.
//!
//! [`NetGameSteam`] adds the plugin with the app id from the plugin's own rules (`Regia.toml`
//! `[steam] app_id`, `STEAM_APP_ID`, `steam_appid.txt`). A `steamworks` build talks to the real
//! Steam client; a `steam` build without `steamworks` runs on `MockSteam`, so the readout can be
//! seen and tested without the SDK. A text node named [`STEAM_LINE_NODE`] in the top left corner
//! shows:
//!
//! - whether Steam is running, the app id and which backend this build uses,
//! - `overlay open` / `overlay closed` (`SteamStatus::overlay_open`, the same value the agent
//!   method `steam.status` answers as `overlay_open`),
//! - the last overlay message the game received (`OverlayOpened` or `OverlayClosed`) and how many
//!   of each arrived,
//! - the GPU and graphics backend the window renders with (`Vulkan`, `Dx12`, ...), so a DX12 run
//!   can be told from a Vulkan one on screen.
//!
//! An agent reads the same line with `ui.query {"name": "steam_overlay"}` and the state itself with
//! `steam.status`.
//!
//! The line's first row is the headline `STEAM OVERLAY: OPEN (opened N, closed M)` /
//! `STEAM OVERLAY: closed (...)` in a larger font (ADR-7349), so the owner's check reads one row.
//! Two more things for that check (ADR-7349): the scoreboard this HUD shows while Tab is held is
//! kept hidden while Shift is down or the overlay is open (Shift+Tab held showed the scoreboard,
//! which looks like an overlay that closes on release), and with `REGIA_STEAM_OVERLAY_TRACE` the
//! trace also records the renderer's hidden-surface throttle (`SurfacesHidden`).

use std::sync::Arc;

use regia::prelude::*;
use regia::steam::{OverlayClosed, OverlayOpened, Steam, SteamBackend, SteamPlugin, SteamState, SteamStatus};
use regia::ui::NodeName;
use regia::ui::style::Align;

/// The `NodeName` of the HUD's Steam line.
pub const STEAM_LINE_NODE: &str = "steam_overlay";

/// What the client has seen of the Steam overlay. Updated in `Update` from the plugin's
/// `OverlayOpened` / `OverlayClosed` messages and `SteamStatus`.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamOverlayReadout {
    /// The overlay is open now (`SteamStatus::overlay_open`).
    pub overlay_open: bool,
    /// `OverlayOpened` messages received since start.
    pub opened: u32,
    /// `OverlayClosed` messages received since start.
    pub closed: u32,
    /// The last overlay message received: `"OverlayOpened"` or `"OverlayClosed"`.
    pub last_event: Option<&'static str>,
    /// The adapter the window renders with, as `name (backend)`, once the render device exists.
    pub gpu: Option<String>,
}

/// Which Steam backend this client runs on, as the HUD names it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamBackendLabel(pub &'static str);

/// Adds `SteamPlugin` and the HUD's Steam line to a client. Add it after
/// [`crate::hud::NetGameHud`]. When the app already has a `SteamPlugin`, that one stays and only
/// the line is added.
pub struct NetGameSteam {
    backend: Option<Arc<dyn SteamBackend>>,
    label: &'static str,
}

impl NetGameSteam {
    /// The backend this build links: the real Steam client under `steamworks`, `MockSteam`
    /// (ready, no events of its own) under `steam` alone.
    #[must_use]
    pub fn for_this_build() -> Self {
        #[cfg(feature = "steamworks")]
        {
            Self {
                backend: None,
                label: "steamworks",
            }
        }
        #[cfg(not(feature = "steamworks"))]
        {
            Self {
                backend: Some(regia::steam::MockSteam::shared()),
                label: "mock",
            }
        }
    }

    /// An explicit backend (tests inject `MockSteam` and its events), shown as `label`.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn SteamBackend>, label: &'static str) -> Self {
        Self {
            backend: Some(backend),
            label,
        }
    }

    /// How the HUD names this plugin's backend (`steamworks`, `mock`, ...).
    #[must_use]
    pub fn backend_label(&self) -> &'static str {
        self.label
    }
}

impl Plugin for NetGameSteam {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<SteamPlugin>()) {
            let mut plugin = SteamPlugin::from_project_settings();
            if let Some(backend) = &self.backend {
                plugin = plugin.with_backend(Arc::clone(backend));
            }
            app.add_plugins(plugin);
        }
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugins(UiPlugin::code_only());
        }
        app.init_resource::<SteamOverlayReadout>()
            .init_resource::<ShiftHeld>()
            .insert_resource(SteamBackendLabel(self.label))
            .add_systems(Startup, spawn_steam_line)
            .add_systems(
                Update,
                // One chain, no one-frame gap: the overlay state is read into `SteamOverlayReadout`
                // and `ShiftHeld` cleared before the board block decides on both, and the line is
                // written last (w9-net-vehicle-steam-z 6).
                (
                    track_overlay,
                    track_shift,
                    block_scores_on_shift_tab,
                    show_steam_line,
                )
                    .chain(),
            )
            .add_systems(
                Last,
                trace_hidden_surfaces.run_if(resource_exists::<regia::steam::SteamOverlayTrace>),
            );
    }
}

/// Marks the HUD's Steam text node.
#[derive(Component)]
struct SteamLine;

fn spawn_steam_line(mut commands: Commands) {
    let root = commands
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                align_items: Align::Start,
                padding: Edges::px(12.0),
                ..Default::default()
            }),
            PickThrough,
        ))
        .id();
    let line = commands
        .spawn((
            UiNode,
            NodeName(STEAM_LINE_NODE.to_owned()),
            SteamLine,
            NodeStyle::new(Style {
                size: [Dimension::Auto, Dimension::Auto],
                padding: Edges::px(6.0),
                ..Default::default()
            }),
            Background {
                kind: regia::ui::BackgroundKind::Color(
                    UiColor::from_hex("#0c1117c0").unwrap_or(UiColor::BLACK),
                ),
            },
            Text::new("Steam: starting"),
            PickThrough,
        ))
        .id();
    commands.entity(root).add_child(line);
}

/// Whether a Shift key is down, from the window's key events. Focus loss and an opening overlay
/// clear it: the key-up goes to whichever window (or overlay) has focus then.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShiftHeld(pub bool);

fn track_shift(
    mut keys: MessageReader<regia::platform::KeyboardInput>,
    mut focus: MessageReader<regia::platform::WindowFocused>,
    mut overlay: MessageReader<OverlayOpened>,
    mut shift: ResMut<ShiftHeld>,
) {
    let mut held = shift.0;
    for event in keys.read() {
        if event.physical_key.starts_with("Shift") {
            held = event.state == regia::platform::ButtonState::Pressed;
        }
    }
    // The Steam overlay draws inside the game's window, so the window keeps focus and the Shift
    // key-up goes to the overlay: without this the hold would stick and keep the scoreboard
    // parked for ever after the overlay closes (w9-net-vehicle-steam-z 4).
    if overlay.read().next().is_some() || focus.read().any(|event| !event.focused) {
        held = false;
    }
    if shift.0 != held {
        shift.0 = held;
    }
}

/// A board's own hold action, kept while the Steam block clears it.
#[derive(Component)]
struct ParkedHoldAction(String);

/// Keeps the Tab scoreboard hidden while Shift is down or the overlay is open: Shift+Tab is
/// Steam's overlay shortcut, and a board that shows while Tab is held looks like an overlay that
/// closes on release (the owner's 2026-11-12 run, ADR-7349). The board's hold action is parked
/// and put back when neither holds.
fn block_scores_on_shift_tab(
    shift: Res<ShiftHeld>,
    readout: Res<SteamOverlayReadout>,
    mut boards: Query<(
        Entity,
        &mut regia::ui::game_widgets::Scoreboard,
        Option<&ParkedHoldAction>,
    )>,
    mut commands: Commands,
) {
    let block = shift.0 || readout.overlay_open;
    for (entity, mut board, parked) in &mut boards {
        match (block, parked) {
            (true, None) if !board.hold_action.is_empty() => {
                let action = std::mem::take(&mut board.hold_action);
                commands.entity(entity).insert(ParkedHoldAction(action));
            }
            (false, Some(parked)) => {
                board.hold_action = parked.0.clone();
                commands.entity(entity).remove::<ParkedHoldAction>();
            }
            _ => {}
        }
    }
}

/// `Last`, while tracing: the renderer's hidden-surface throttle, which stops presenting (and so
/// stops the overlay from drawing) when the compositor stops asking for frames.
fn trace_hidden_surfaces(
    hidden: Option<Res<regia::platform::SurfacesHidden>>,
    mut trace: ResMut<regia::steam::SteamOverlayTrace>,
    mut seen: Local<Option<bool>>,
) {
    let now = hidden.is_some_and(|h| h.0);
    if *seen != Some(now) {
        *seen = Some(now);
        trace.note(format_args!(
            "renderer surfaces hidden (throttled, not presenting): {now}"
        ));
    }
}

/// Counts the overlay messages, mirrors `SteamStatus::overlay_open` and notes the adapter once.
fn track_overlay(
    mut opened: MessageReader<OverlayOpened>,
    mut closed: MessageReader<OverlayClosed>,
    status: Res<SteamStatus>,
    device: Option<Res<regia_render::RenderDevice>>,
    mut readout: ResMut<SteamOverlayReadout>,
) {
    let opened_now = opened.read().count();
    let closed_now = closed.read().count();
    readout.opened = readout
        .opened
        .saturating_add(u32::try_from(opened_now).unwrap_or(u32::MAX));
    readout.closed = readout
        .closed
        .saturating_add(u32::try_from(closed_now).unwrap_or(u32::MAX));
    // Both can arrive in one frame (Steam batched the callbacks): the state after the batch says
    // which came last.
    let last = match (opened_now > 0, closed_now > 0) {
        (true, true) if status.overlay_open => Some("OverlayOpened"),
        (true, true) | (false, true) => Some("OverlayClosed"),
        (true, false) => Some("OverlayOpened"),
        (false, false) => None,
    };
    if last.is_some() {
        readout.last_event = last;
    }
    if readout.overlay_open != status.overlay_open {
        readout.overlay_open = status.overlay_open;
    }
    if readout.gpu.is_none()
        && let Some(device) = device
    {
        let info = device.adapter.get_info();
        readout.gpu = Some(format!("{} ({:?})", info.name, info.backend));
    }
}

/// Rewrites the HUD line when what it shows changed.
fn show_steam_line(
    readout: Res<SteamOverlayReadout>,
    status: Res<SteamStatus>,
    steam: Res<Steam>,
    label: Res<SteamBackendLabel>,
    mut shown: Local<Option<(String, bool)>>,
    mut lines: Query<&mut Text, With<SteamLine>>,
) {
    let text = steam_line(steam.state(), &status, &readout, label.0);
    let open = readout.overlay_open;
    if shown.as_ref().is_some_and(|(t, o)| *t == text && *o == open) {
        return;
    }
    let color = if open {
        UiColor([1.0, 0.85, 0.2, 1.0])
    } else {
        UiColor::WHITE
    };
    for mut line in &mut lines {
        *line = Text::new(text.clone()).with_style(TextStyle {
            font_px: 18.0,
            color,
            ..Default::default()
        });
    }
    if !lines.is_empty() {
        *shown = Some((text, open));
    }
}

/// The HUD's Steam line for `state`, `status` and what the client saw of the overlay, on the
/// backend named `backend`.
#[must_use]
pub fn steam_line(
    state: SteamState,
    status: &SteamStatus,
    readout: &SteamOverlayReadout,
    backend: &str,
) -> String {
    // The headline first (ADR-7349): the one row the owner's overlay check reads.
    let mut text = format!(
        "STEAM OVERLAY: {} (opened {}, closed {})\n",
        if status.overlay_open { "OPEN" } else { "closed" },
        readout.opened,
        readout.closed
    );
    text.push_str(&match state {
        SteamState::Ready => {
            let app = status
                .app_id
                .map_or_else(|| "app ?".to_owned(), |id| format!("app {}", id.0));
            let offline = if status.offline { ", offline mode" } else { "" };
            let overlay = if status.overlay_open {
                "overlay OPEN (Shift+Tab closes it)"
            } else {
                "overlay closed (Shift+Tab opens it)"
            };
            format!("Steam ({backend}): running, {app}{offline}\n{overlay}")
        }
        SteamState::Unavailable => format!(
            "Steam ({backend}): not running\nstart the Steam client, sign in, set STEAM_APP_ID; the game runs without Steam"
        ),
        SteamState::Restarting => format!("Steam ({backend}): relaunching through the Steam client"),
    });
    match readout.last_event {
        Some(event) => text.push_str(&format!(
            "\nlast: {event} (opened {}, closed {})",
            readout.opened, readout.closed
        )),
        None => text.push_str("\nlast: no overlay message yet"),
    }
    if let Some(gpu) = &readout.gpu {
        text.push_str(&format!("\ngpu: {gpu}"));
    }
    text
}

/// The client binary's `--list-adapters` (Steam builds): when `args` holds it, removes it, prints
/// every adapter with the index `REGIA_GPU_INDEX` takes (Vulkan first, then DX12 on Windows; the
/// numbering of `regia_render::list_adapters`) and returns the exit code; otherwise `None`.
pub fn list_adapters_flag(args: &mut Vec<String>) -> Option<std::process::ExitCode> {
    let before = args.len();
    args.retain(|arg| arg != "--list-adapters");
    if args.len() == before {
        return None;
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for adapter in regia_render::list_adapters() {
        // A closed pipe is not an error worth a panic.
        let _ = writeln!(out, "adapter {adapter}");
    }
    let _ = out.flush();
    Some(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia::steam::AppId;

    fn ready(overlay_open: bool) -> SteamStatus {
        SteamStatus {
            running: true,
            app_id: Some(AppId(480)),
            overlay_open,
            ..SteamStatus::default()
        }
    }

    #[test]
    fn the_line_names_the_backend_the_app_and_the_overlay_state() {
        let line = steam_line(
            SteamState::Ready,
            &ready(false),
            &SteamOverlayReadout::default(),
            "steamworks",
        );
        assert_eq!(
            line,
            "STEAM OVERLAY: closed (opened 0, closed 0)\nSteam (steamworks): running, app 480\noverlay closed (Shift+Tab opens it)\nlast: no overlay message yet"
        );
        let readout = SteamOverlayReadout {
            overlay_open: true,
            opened: 1,
            closed: 0,
            last_event: Some("OverlayOpened"),
            gpu: Some("NVIDIA GeForce RTX 3080 (Dx12)".to_owned()),
        };
        let line = steam_line(SteamState::Ready, &ready(true), &readout, "steamworks");
        assert!(
            line.starts_with("STEAM OVERLAY: OPEN (opened 1, closed 0)\n"),
            "the headline is the first row: {line}"
        );
        assert!(line.contains("overlay OPEN"), "{line}");
        assert!(
            line.contains("last: OverlayOpened (opened 1, closed 0)"),
            "{line}"
        );
        assert!(line.ends_with("gpu: NVIDIA GeForce RTX 3080 (Dx12)"), "{line}");
    }

    #[test]
    fn a_missing_client_and_a_relaunch_say_so() {
        let line = steam_line(
            SteamState::Unavailable,
            &SteamStatus::default(),
            &SteamOverlayReadout::default(),
            "steamworks",
        );
        assert!(
            line.starts_with("STEAM OVERLAY: closed (opened 0, closed 0)\nSteam (steamworks): not running\n"),
            "{line}"
        );
        let line = steam_line(
            SteamState::Restarting,
            &SteamStatus::default(),
            &SteamOverlayReadout::default(),
            "steamworks",
        );
        assert!(line.contains("relaunching"), "{line}");
        let offline = SteamStatus {
            offline: true,
            ..ready(false)
        };
        let line = steam_line(
            SteamState::Ready,
            &offline,
            &SteamOverlayReadout::default(),
            "mock",
        );
        assert!(
            line.contains("\nSteam (mock): running, app 480, offline mode"),
            "{line}"
        );
    }

    #[test]
    fn this_build_picks_its_backend() {
        let expected = if cfg!(feature = "steamworks") {
            "steamworks"
        } else {
            "mock"
        };
        assert_eq!(NetGameSteam::for_this_build().backend_label(), expected);
    }
}
