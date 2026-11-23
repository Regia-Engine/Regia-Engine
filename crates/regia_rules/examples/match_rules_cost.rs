// SPDX-License-Identifier: MPL-2.0
//! Step cost of the match rules for a grid of contestants and objectives.
//!
//! Prints one JSON object on stdout. An optional path argument is written atomically (a temporary
//! file beside the destination, then rename). The numbers are evidence. Nothing here asserts a
//! budget. The `zone_elimination` row times the shrinking zone and the team eliminations
//! (GR-11.3) for 100 solo contestants, half of them outside the circle, one team out every other
//! step.
//!
//! ```text
//! cargo run -p regia_rules --example match_rules_cost -- path/to/match_rules_cost.json
//! ```

// A host-side timing probe, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::path::Path;
use std::time::Instant;

use regia_rules::match_flow::{FlowSettings, SeatId};
use regia_rules::mode::MatchMode;
use regia_rules::objective::{Contestant, HillVolume, ObjectiveSettings};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::zone::{ZonePhase, ZoneSchedule};

const CONTESTANTS: [usize; 4] = [8, 64, 512, 4096];
const OBJECTIVES: [usize; 3] = [2, 8, 32];
const STEPS: u32 = 64;
const WARMUP: u32 = 8;

fn main() {
    if let Err(err) = run() {
        eprintln!("match_rules_cost: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut rows = Vec::new();
    for contestants in CONTESTANTS {
        for objectives in OBJECTIVES {
            for mode in [MatchMode::CaptureTheFlag, MatchMode::KingOfTheHill] {
                rows.push(measure(contestants, objectives, mode)?);
            }
        }
    }
    rows.push(measure_zone_elimination(100)?);
    let body = serde_json::json!({
        "example": "match_rules_cost",
        "host_arch": std::env::consts::ARCH,
        "host_os": std::env::consts::OS,
        "load": load_line(),
        "zones": "row genre=zone_elimination (100 solo teams)",
        "steps_timed": STEPS,
        "warmup_steps": WARMUP,
        "budget": {
            "ms_per_step": 0.05,
            "contestants": 64,
            "objectives": 8,
            "host": "N150 laptop",
            "asserted": false
        },
        "rows": rows,
    });
    let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
    println!("{text}");
    if let Some(path) = std::env::args().nth(1) {
        write_atomic(Path::new(&path), text.as_bytes())?;
    }
    Ok(())
}

fn measure(contestants: usize, objectives: usize, mode: MatchMode) -> Result<serde_json::Value, String> {
    let team_cap = u8::try_from(objectives.max(2)).unwrap_or(u8::MAX);
    let objective_cap = u16::try_from(objectives).unwrap_or(u16::MAX);
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            max_teams: team_cap,
            start_delay_ticks: 0,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            max_teams: team_cap,
            max_objectives: objective_cap,
            capture_limit: u32::MAX,
            hill_score_limit: u32::MAX,
            hill_accrual_ticks: 1,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    session.start_now();
    session.board.reset(mode);
    let placed = match mode {
        MatchMode::CaptureTheFlag => {
            let teams: Vec<u8> = (1..=team_cap).collect();
            session
                .board
                .place_flags(&teams, 40.0, 1)
                .map_err(|e| e.to_string())?
        }
        MatchMode::KingOfTheHill => {
            session
                .board
                .install_hill_volume(
                    1,
                    HillVolume {
                        min: [-2.0, -1.0, -2.0],
                        max: [2.0, 2.0, 2.0],
                    },
                )
                .map_err(|e| e.to_string())?;
            1usize
        }
        _ => 0usize,
    };
    let units = contestants_for(contestants, team_cap);
    for _ in 0..WARMUP {
        let _ = session.step(&units);
    }
    let started = Instant::now();
    for _ in 0..STEPS {
        let _ = session.step(&units);
    }
    let elapsed_ns = started.elapsed().as_nanos();
    let ns_per_step = elapsed_ns as f64 / f64::from(STEPS);
    Ok(serde_json::json!({
        "mode": mode.as_lobby_str(),
        "contestants": contestants,
        "objectives_requested": objectives,
        "objectives_placed": placed,
        "ns_per_step": ns_per_step,
        "ms_per_step": ns_per_step / 1_000_000.0,
    }))
}

fn measure_zone_elimination(contestants: u8) -> Result<serde_json::Value, String> {
    let zone = ZoneSchedule {
        center: [0.0, 0.0],
        radius: 60.0,
        phases: vec![ZonePhase {
            wait_ticks: 4,
            shrink_ticks: 1000,
            end_radius: 10.0,
            damage_per_tick: 1,
        }],
        seed: 7,
    };
    let mut settings = regia_rules::elimination::last_standing_settings(contestants, 1, Some(zone));
    settings.flow.start_delay_ticks = 0;
    let mut session = MatchSession::new(settings);
    for team in 1..=contestants {
        session
            .apply(&regia_rules::match_flow::FlowInput::Seat { team, slot: 0 })
            .map_err(|e| e.to_string())?;
    }
    session.start_now();
    // Contestants on a line through the center: about half start outside the 60-unit circle.
    let units: Vec<Contestant> = (1..=contestants)
        .map(|team| Contestant {
            id: u32::from(team),
            team,
            position: [f32::from(team) * 1.2 - 60.0, 0.0, f32::from(team) * 1.2 - 60.0],
            alive: true,
        })
        .collect();
    for _ in 0..WARMUP {
        let _ = session.step(&units);
    }
    let mut events = 0usize;
    let mut next_out = 1u8;
    let started = Instant::now();
    for step in 0..STEPS {
        if step.is_multiple_of(2) && next_out < contestants {
            session.note_death(SeatId::new(next_out, 0));
            next_out += 1;
        }
        events += session.step(&units).events.len();
    }
    let elapsed_ns = started.elapsed().as_nanos();
    let ns_per_step = elapsed_ns as f64 / f64::from(STEPS);
    Ok(serde_json::json!({
        "mode": "zone_elimination",
        "genre": "zone_elimination",
        "contestants": contestants,
        "teams_out": session.placements.out.len(),
        "events": events,
        "ns_per_step": ns_per_step,
        "ms_per_step": ns_per_step / 1_000_000.0,
    }))
}

fn contestants_for(count: usize, teams: u8) -> Vec<Contestant> {
    let span = usize::from(teams.max(1));
    let mut units = Vec::with_capacity(count);
    for index in 0..count {
        let id = u32::try_from(index).unwrap_or(u32::MAX);
        let team = u8::try_from((index % span).saturating_add(1)).unwrap_or(1);
        let x = f32::from(u8::try_from(index % 64).unwrap_or(0)) - 32.0;
        let z = f32::from(u16::try_from(index / 64).unwrap_or(0));
        units.push(Contestant {
            id,
            team,
            position: [x, 0.0, z],
            alive: true,
        });
    }
    units
}

fn load_line() -> String {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim().to_string(),
        Err(_) => "not measured".to_string(),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}
