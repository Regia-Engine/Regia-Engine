// SPDX-License-Identifier: MIT-0
//! The owner's N08 leg: the `net_game` template played over real Steam lobbies and sockets by two
//! accounts on two machines (`book/user-guide/steam-real-run.md` § 4a, ADR-5375). Ignored: a
//! normal `cargo test` lists it as `ignored`, which means not run, never a pass. Run it with
//! `--ignored` and the setup below on each machine; without the setup it fails `NOT RUN (<code>)`.
//!
//! ```console
//! REGIA_STEAM_NET_LEG=host REGIA_STEAM_NET_LOG=$PWD/net-host.log \
//!     cargo test -p net_game --features steamworks --test steam_leg_real -- --ignored --nocapture
//! REGIA_STEAM_NET_LEG=guest REGIA_STEAM_LOBBY_ID=<printed id> REGIA_STEAM_NET_LOG=$PWD/net-guest.log \
//!     cargo test -p net_game --features steamworks --test steam_leg_real -- --ignored --nocapture
//! ```
//!
//! Both need `STEAM_APP_ID` (default 480) and `REGIA_STEAM_RUN_COMMIT=$(git rev-parse HEAD)`.

use net_game::link_options;
use net_game::steam_leg::SteamLegEndpoint;
use regia::net::ConditionerConfig;
use regia::net::link::LinkClock;
use regia::steam::real_run::{EndpointStatus, RealRunRole};
use regia::steam::real_run_net::{NET_LOG_ENV, net_leg_setup_from_env};
use regia::steam::{RealBackend, SteamBackend};

/// Polls before giving up: one poll per fixed step at 60 Hz, about 7 minutes (the guest has
/// about 4 minutes to start after the host printed the lobby id).
const WAIT_POLLS: u32 = 25_000;

#[test]
#[ignore = "owner session only: two machines, two signed-in Steam accounts; see steam-real-run.md § 4a"]
fn two_account_template_match_over_steam() {
    let setup = match net_leg_setup_from_env() {
        Ok(setup) => setup,
        Err(not_run) => panic!("{not_run}"),
    };
    let role = setup.role;
    let log_path = std::path::PathBuf::from(
        regia::core::brand::env_var(NET_LOG_ENV)
            .unwrap_or_else(|_| format!("regia-steam-net-leg-{}.log", role.name())),
    );
    let backend = RealBackend::offline();
    if let Err(err) = backend.initialize(Some(setup.app_id)) {
        panic!("NOT RUN (steam_not_ready): a running, signed-in Steam client is needed: {err}");
    }
    let agent_dir = std::env::temp_dir().join(format!(
        "regia-steam-net-leg-{}-{}",
        role.name(),
        std::process::id()
    ));
    if let Err(err) = std::fs::create_dir_all(&agent_dir) {
        panic!(
            "NOT RUN (setup_absent): cannot create {}: {err}",
            agent_dir.display()
        );
    }
    let opts = link_options(LinkClock::Wall, ConditionerConfig::default());
    let mut endpoint = match SteamLegEndpoint::start(setup, &backend, agent_dir.clone(), opts) {
        Ok(endpoint) => endpoint,
        Err(code) => panic!("NOT RUN ({code}): the endpoint did not start"),
    };
    let mut printed = 0;
    let mut announced = false;
    let mut status = EndpointStatus::Running;
    // Deadline pacing at 60 Hz (as the template client's `pace`): a poll is one fixed step, so a
    // fixed sleep after each poll's work would run each machine slower than 60 Hz, by a different
    // amount on each. A late step does not bank a burst; the next deadline starts from now.
    let step = std::time::Duration::from_micros(16_667);
    let mut next = std::time::Instant::now();
    for _ in 0..WAIT_POLLS {
        let _ = backend.pump();
        status = endpoint.poll();
        if role == RealRunRole::Host
            && !announced
            && let Some(lobby) = endpoint.lobby()
        {
            println!("REGIA_STEAM_LOBBY_ID={}", lobby.0);
            announced = true;
        }
        for line in &endpoint.log().lines()[printed..] {
            println!("REGIA_NET {line}");
        }
        printed = endpoint.log().lines().len();
        if status != EndpointStatus::Running {
            break;
        }
        next += step;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    if status == EndpointStatus::Running {
        endpoint.time_out();
        for line in &endpoint.log().lines()[printed..] {
            println!("REGIA_NET {line}");
        }
    }
    match endpoint.log().write_to(&log_path) {
        Ok(()) => println!("REGIA_NET_LOG={}", log_path.display()),
        Err(err) => eprintln!("could not write {}: {err}", log_path.display()),
    }
    drop(endpoint);
    let _ = std::fs::remove_dir_all(&agent_dir);
    assert_eq!(
        status_of(&log_path),
        Some(true),
        "the {} endpoint did not complete; its log is {}",
        role.name(),
        log_path.display()
    );
}

/// Whether the written log ends complete (`None` when it cannot be read).
fn status_of(path: &std::path::Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.lines().any(|l| l == "result status=complete"))
}
