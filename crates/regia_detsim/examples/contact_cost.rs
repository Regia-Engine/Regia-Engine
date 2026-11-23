// SPDX-License-Identifier: MPL-2.0
//! Time the contact pass (ADR-8563): the `contact_pile` scenario with N bodies, through the grid or
//! the all-pairs loop, measured by `regia_detsim::profile` (p50/p99/max ns per tick; prints, asserts
//! nothing): `cargo run --release -p regia_detsim --example contact_cost -- [bodies] [samples]
//! [warmup] [--n2]`. Defaults: 900 bodies, 600 samples, 60 warmup ticks.

use std::process::ExitCode;

use regia_detsim::contact::{ContactPile, ContactStats};
use regia_detsim::golden::{DetScenario, TickRate};
use regia_detsim::profile::{self, ProfileConfig};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("regia-contact-cost-error {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[expect(
        clippy::disallowed_methods,
        reason = "timing probe: command-line arguments, read once before any sim tick (ADR-8367)"
    )]
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n2 = args.iter().any(|a| a == "--n2");
    let mut numbers = args.iter().filter(|a| *a != "--n2");
    let mut next = |default: u32| -> Result<u32, Box<dyn std::error::Error>> {
        match numbers.next() {
            Some(v) => Ok(v.parse()?),
            None => Ok(default),
        }
    };
    let bodies = next(900)? as usize;
    let samples = next(600)?;
    let warmup = next(60)?;

    let make = || {
        let mut pile = ContactPile::with_bodies("contact_cost", bodies);
        pile.use_reference_loop(n2);
        pile
    };
    let seed = make().default_run().seed;
    let config = ProfileConfig {
        seed,
        rate: TickRate::DEFAULT,
        warmup_ticks: warmup,
        samples,
    };
    let timing = profile::run(make(), config)?;

    // The counters of the same ticks, from an untimed run.
    let mut pile = make();
    let mut run = pile.default_run();
    run.ticks = warmup + samples;
    pile.reset(&run);
    let mut measured = ContactStats::default();
    for tick in 0..run.ticks {
        pile.step(tick);
        if tick >= warmup {
            measured.add(&pile.last_stats());
        }
    }
    let per_tick = |v: u64| v / u64::from(samples.max(1));
    println!(
        "regia-contact-cost {{\"bodies\":{bodies},\"loop\":\"{}\",\"warmup\":{warmup},\"samples\":{samples},\
         \"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"pairs_per_tick\":{},\"pushes_per_tick\":{},\
         \"capped_per_tick\":{},\"rams\":{},\"arch\":\"{}\",\"os\":\"{}\"}}",
        if n2 { "n2" } else { "grid" },
        timing.p50_ns,
        timing.p99_ns,
        timing.max_ns,
        per_tick(measured.pairs),
        per_tick(measured.pushes),
        per_tick(measured.capped_pushes),
        measured.rams,
        std::env::consts::ARCH,
        std::env::consts::OS,
    );
    Ok(())
}
