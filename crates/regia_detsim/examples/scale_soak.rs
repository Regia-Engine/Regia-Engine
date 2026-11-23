// SPDX-License-Identifier: MPL-2.0
//! CPU-only scale runner; see book/agents/det-scale-soak.md for the JSON contract.
//! `--ai` runs the AI-on soak (ADR-8561); see the page's "AI-on soak" section.
// The allocation counter of `--ai --long` is a counting global allocator: it forwards every
// call to the system allocator and adds two atomic counter updates (ADR-8561).
#![allow(unsafe_code)]
use std::process::ExitCode;

use regia_detsim::golden::{self, DetScenario};
use regia_detsim::scale_soak::{ScaleScenario, SoakConfig, profile};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Display preserves the stable code of typed library errors; Result's
            // default termination would print their Debug type names instead.
            eprintln!("regia-scale-soak-error {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    regia_detsim::fpu::ensure_default_fp_env();
    let mut config = SoakConfig::default();
    let mut ticks = 600_u32;
    let mut warmup = 120_u32;
    let mut seed = 0x5eed_0900_0040;
    let mut budget = profile::DEFAULT_BUDGET_NS;
    let mut expected = None;
    let mut verify = false;
    #[expect(
        clippy::disallowed_methods,
        reason = "soak probe: command-line arguments, read once before any sim tick (ADR-8367)"
    )]
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--ai") {
        return ai::run(argv.into_iter().skip(2));
    }
    let mut args = argv.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--verify" {
            verify = true;
            continue;
        }
        if arg == "--help" {
            println!(
                "scale_soak [--ai ...] [--ticks N] [--warmup N] [--seed HEX] [--seats N] [--entities N] [--budget-ns N] [--expected-hash HEX] [--verify]"
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("invalid_arguments: missing value for {arg}"))?;
        match arg.as_str() {
            "--ticks" => ticks = number(&value)?,
            "--warmup" => warmup = number(&value)?,
            "--seats" => config.seats = number(&value)?,
            "--entities" => config.entities = number(&value)?,
            "--seed" => seed = hex(&value)?,
            "--budget-ns" => budget = number(&value)?,
            "--expected-hash" => expected = Some(hex(&value)?),
            _ => return Err(format!("invalid_arguments: unknown option {arg}").into()),
        }
    }
    let mut scenario = ScaleScenario::new(config)?;
    let run = golden::DetRun {
        seed,
        ticks,
        ..scenario.default_run()
    };
    let measured = profile::measure(&mut scenario, run, warmup, budget)?;
    println!("{}{}", profile::REPORT_PREFIX, measured.to_json());
    if let Some(expected) = expected
        && measured.trace.run_hash() != expected
    {
        return Err(format!(
            "hash_mismatch: expected {expected:016x}, got {:016x}",
            measured.trace.run_hash()
        )
        .into());
    }
    if verify {
        let mut reference = ScaleScenario::new(config)?;
        reference.reset(&measured.run);
        for (tick, &want) in measured.trace.tick_hashes.iter().enumerate() {
            reference.step_reference(tick as u32);
            let got = reference.fingerprint();
            if got != want {
                return Err(format!(
                    "diverged: first_diverging_tick={tick}, grid={want:016x}, reference={got:016x}"
                )
                .into());
            }
        }
        println!(
            "regia-scale-soak-check {{\"status\":\"ok\",\"ticks\":{},\"hash\":\"{:016x}\"}}",
            measured.run.ticks,
            measured.trace.run_hash()
        );
    }
    Ok(())
}

fn number<T: std::str::FromStr>(value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid_arguments: invalid integer {value:?}"))
}
fn hex(value: &str) -> Result<u64, String> {
    u64::from_str_radix(value.trim_start_matches("0x"), 16)
        .map_err(|_| format!("invalid_arguments: invalid hexadecimal integer {value:?}"))
}

/// Counting global allocator for the long mode's allocation samples.
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
    pub static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;

    // SAFETY: forwards every call to the system allocator unchanged; the only addition is
    // relaxed atomic counter updates, which neither allocate nor touch the returned memory.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            // SAFETY: same contract as the caller's.
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            // SAFETY: same contract as the caller's.
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            // SAFETY: same contract as the caller's.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            // SAFETY: same contract as the caller's.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;
}

/// `--ai`: the AI-on soak runner.
mod ai {
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;

    use regia_detsim::golden::{self, DetRun, DetScenario};
    use regia_detsim::profile::{ClockKind, HostClock, ProfileClock};
    use regia_detsim::scale_soak::ai::{
        self, AiQuery, AiSoakConfig, AiSoakMeasurement, AiSoakRunner, AiSoakScenario,
    };

    use super::{counting, hex, number};

    const HELP: &str = "scale_soak --ai [--ticks N] [--warmup N] [--seed HEX] [--teams N] [--seats N] \
[--entities N] [--shots-per-tick N] [--pass-line-ns N] [--clock thread_cpu|wall] [--expected-hash HEX] [--verify] \
[--curve N,N,...] [--long]";

    struct Options {
        config: AiSoakConfig,
        ticks: u32,
        warmup: u32,
        seed: Option<u64>,
        pass_line: u64,
        expected: Option<u64>,
        verify: bool,
        curve: Vec<u16>,
        long: bool,
        clock: ClockChoice,
    }

    /// `--clock`: the thread CPU clock (the default; wall time where the platform has
    /// none) or monotonic wall time. Windows' thread clock advances in scheduler quanta
    /// (about 15.6 ms), too coarse for one tick, so Windows runs use `--clock wall`.
    #[derive(Clone, Copy)]
    enum ClockChoice {
        ThreadCpu,
        Wall,
    }

    impl ClockChoice {
        #[expect(
            clippy::disallowed_methods,
            reason = "soak probe: the wall-clock fallback times passes outside the sim state; nothing simulated reads it (ADR-8367)"
        )]
        fn make(self) -> SoakClock {
            match self {
                Self::ThreadCpu => SoakClock::Host(HostClock::probe()),
                // det-scan: allow(wall_clock): profiling only, never hashed
                Self::Wall => SoakClock::Wall(std::time::Instant::now()),
            }
        }
    }

    enum SoakClock {
        Host(HostClock),
        Wall(std::time::Instant),
    }

    impl ProfileClock for SoakClock {
        fn kind(&self) -> ClockKind {
            match self {
                Self::Host(clock) => clock.kind(),
                Self::Wall(_) => ClockKind::Wall,
            }
        }
        fn now_ns(&self) -> u64 {
            match self {
                Self::Host(clock) => clock.now_ns(),
                // det-scan: allow(wall_clock): profiling only, never hashed
                Self::Wall(origin) => u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX),
            }
        }
    }

    pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(options) = parse(args)? else {
            println!("{HELP}");
            return Ok(());
        };
        if !options.curve.is_empty() {
            return curve(&options);
        }
        let scenario = AiSoakScenario::new(options.config)?;
        let run = DetRun {
            seed: options.seed.unwrap_or(scenario.default_run().seed),
            ticks: options.ticks,
            ..scenario.default_run()
        };
        let mut runner = AiSoakRunner::with_clock(
            scenario,
            run,
            options.warmup,
            options.pass_line,
            options.clock.make(),
        )?;
        let mut memory = Vec::new();
        if options.long {
            memory.push(sample("start", 0));
        }
        let minute = options.warmup.saturating_add(3_600);
        if options.long && options.warmup == 0 {
            memory.push(sample("warm", 0));
        }
        while runner.step() {
            if options.long && options.warmup > 0 && runner.next_tick() == options.warmup {
                memory.push(sample("warm", options.warmup));
            }
            if options.long && runner.next_tick() == minute && runner.remaining() > 0 {
                memory.push(sample("one_minute", minute));
            }
        }
        if options.long {
            memory.push(sample("end", runner.next_tick()));
        }
        let measured = runner.finish();
        println!("{}{}", ai::REPORT_PREFIX, report_json(&measured));
        if options.long {
            println!("regia-ai-soak-memory {}", memory_json(&memory));
        }
        let hash = measured.trace.run_hash();
        if let Some(expected) = options.expected
            && hash != expected
        {
            return Err(format!("hash_mismatch: expected {expected:016x}, got {hash:016x}").into());
        }
        if options.verify {
            verify(&options, &measured)?;
        }
        Ok(())
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
        let mut o = Options {
            config: AiSoakConfig::default(),
            ticks: 600,
            warmup: 120,
            seed: None,
            pass_line: ai::PASS_LINE_HOST_NS,
            clock: ClockChoice::ThreadCpu,
            expected: None,
            verify: false,
            curve: Vec::new(),
            long: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" => return Ok(None),
                "--verify" => {
                    o.verify = true;
                    continue;
                }
                "--long" => {
                    o.long = true;
                    continue;
                }
                _ => {}
            }
            let value = args
                .next()
                .ok_or_else(|| format!("invalid_arguments: missing value for {arg}"))?;
            match arg.as_str() {
                "--ticks" => o.ticks = number(&value)?,
                "--warmup" => o.warmup = number(&value)?,
                "--seed" => o.seed = Some(hex(&value)?),
                "--teams" => o.config.teams = number(&value)?,
                "--seats" => o.config.seats = number(&value)?,
                "--entities" => o.config.entities = number(&value)?,
                "--shots-per-tick" => o.config.shots_per_tick = number(&value)?,
                "--pass-line-ns" => o.pass_line = number(&value)?,
                "--clock" => {
                    o.clock = match value.as_str() {
                        "thread_cpu" => ClockChoice::ThreadCpu,
                        "wall" => ClockChoice::Wall,
                        _ => {
                            return Err(format!(
                                "invalid_arguments: --clock takes thread_cpu or wall, not {value:?}"
                            ));
                        }
                    }
                }
                "--expected-hash" => o.expected = Some(hex(&value)?),
                "--curve" => {
                    o.curve = value.split(',').map(number).collect::<Result<_, _>>()?;
                    if o.curve.len() < 2 {
                        return Err("invalid_arguments: --curve needs at least two counts".into());
                    }
                }
                _ => return Err(format!("invalid_arguments: unknown option {arg}")),
            }
        }
        Ok(Some(o))
    }

    /// Replay the measured run with the linear-scan oracle on every query.
    fn verify(options: &Options, measured: &AiSoakMeasurement) -> Result<(), Box<dyn std::error::Error>> {
        let mut checked = AiSoakScenario::new(options.config)?;
        checked.set_oracle(true);
        checked.reset(&measured.trace.run);
        for (tick, &want) in measured.trace.tick_hashes.iter().enumerate() {
            checked.step(tick as u32);
            if let Some(first) = checked.oracle().and_then(|o| o.first.clone()) {
                return Err(format!(
                    "oracle_mismatch: tick={}, query={}, subject={}, grid={:?}, linear={:?}",
                    first.tick,
                    first.query.name(),
                    first.subject,
                    first.grid,
                    first.linear
                )
                .into());
            }
            let got = checked.fingerprint();
            if got != want {
                return Err(format!(
                    "diverged: first_diverging_tick={tick}, measured={want:016x}, oracle={got:016x}"
                )
                .into());
            }
        }
        let checked_counts = checked.oracle().map(|o| o.checked).unwrap_or_default();
        let kinds = [
            AiQuery::Acquire,
            AiQuery::Target,
            AiQuery::Ally,
            AiQuery::KeepOut,
            AiQuery::Segment,
            AiQuery::Burst,
        ];
        let counts = kinds
            .iter()
            .zip(checked_counts)
            .map(|(k, n)| format!("\"{}\":{n}", k.name()))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "regia-ai-soak-check {{\"status\":\"ok\",\"ticks\":{},\"hash\":\"{:016x}\",\"queries\":{{{counts}}}}}",
            measured.trace.run.ticks,
            measured.trace.run_hash()
        );
        Ok(())
    }

    /// Run each entity count and fit `p = a * n^b` by least squares on logarithms.
    fn curve(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
        let mut points = Vec::new();
        for &entities in &options.curve {
            let config = AiSoakConfig {
                entities,
                seats: options.config.seats.min(entities),
                ..options.config
            };
            let scenario = AiSoakScenario::new(config)?;
            let run = DetRun {
                seed: options.seed.unwrap_or(scenario.default_run().seed),
                ticks: options.ticks,
                ..scenario.default_run()
            };
            let measured = AiSoakRunner::with_clock(
                scenario,
                run,
                options.warmup,
                options.pass_line,
                options.clock.make(),
            )?
            .finish();
            println!("{}{}", ai::REPORT_PREFIX, report_json(&measured));
            points.push((entities, measured.report.tick.p50_ns, measured.report.tick.p99_ns));
        }
        let fit = |pick: fn(&(u16, u64, u64)) -> u64| -> f64 {
            let xs: Vec<f64> = points.iter().map(|p| libm::log(f64::from(p.0))).collect();
            let ys: Vec<f64> = points.iter().map(|p| libm::log(pick(p).max(1) as f64)).collect();
            let n = xs.len() as f64;
            let mx = xs.iter().sum::<f64>() / n;
            let my = ys.iter().sum::<f64>() / n;
            let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
            let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
            if sxx > 0.0 { sxy / sxx } else { 0.0 }
        };
        let rows = points
            .iter()
            .map(|(e, p50, p99)| format!("{{\"entities\":{e},\"p50_ns\":{p50},\"p99_ns\":{p99}}}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "regia-ai-soak-curve {{\"target\":\"{}\",\"points\":[{rows}],\"exponent_p50\":{:.3},\"exponent_p99\":{:.3}}}",
            golden::current_target(),
            fit(|p| p.1),
            fit(|p| p.2)
        );
        Ok(())
    }

    fn report_json(m: &AiSoakMeasurement) -> String {
        let c = m.config;
        let k = m.counters;
        let systems = serde_json::to_string(&m.report.systems).unwrap_or_else(|_| "[]".into());
        let tick = serde_json::to_string(&m.report.tick).unwrap_or_else(|_| "{}".into());
        let mut over = String::new();
        for (i, row) in m.report.over_budget.iter().take(32).enumerate() {
            if i > 0 {
                over.push(',');
            }
            let _ = write!(over, "{}", row.tick);
        }
        format!(
            "{{\"scenario\":\"ai_soak\",\"clock\":\"{}\",\"target\":\"{}\",\"seed\":\"{:016x}\",\"teams\":{},\"seats\":{},\
\"entities\":{},\"shots_per_tick\":{},\"max_shots\":{},\"ticks\":{},\"warmup\":{},\"rate_hz\":{},\"pass_line_ns\":{},\
\"status\":\"{}\",\"hash\":\"{:016x}\",\"tick\":{tick},\"systems\":{systems},\"over_line_count\":{},\
\"over_line_ticks_first\":[{over}],\"counters\":{{\"acquired\":{},\"targets\":{},\"allies\":{},\"keep_out\":{},\
\"shots_fired\":{},\"sweeps\":{},\"hits\":{},\"burst_hits\":{},\"recycled\":{},\"contacts\":{}}}}}",
            m.clock().as_str(),
            golden::current_target(),
            m.trace.run.seed,
            c.teams,
            c.seats,
            c.entities,
            c.shots_per_tick,
            c.max_shots,
            m.trace.run.ticks - m.warmup,
            m.warmup,
            m.trace.run.rate.hz(),
            m.pass_line_ns,
            if m.inside_pass_line() { "inside" } else { "over" },
            m.trace.run_hash(),
            m.report.over_budget.len(),
            k.acquired,
            k.targets,
            k.allies,
            k.keep_out,
            k.shots_fired,
            k.sweeps,
            k.hits,
            k.burst_hits,
            k.recycled,
            k.contacts,
        )
    }

    struct Sample {
        label: &'static str,
        tick: u32,
        rss_bytes: Option<u64>,
        allocations: u64,
        live_bytes: u64,
    }

    fn sample(label: &'static str, tick: u32) -> Sample {
        Sample {
            label,
            tick,
            rss_bytes: rss_bytes(),
            allocations: counting::ALLOCATIONS.load(Ordering::Relaxed),
            live_bytes: counting::LIVE_BYTES.load(Ordering::Relaxed),
        }
    }

    /// Resident set size from `/proc/self/status` (Linux, Android); `None` elsewhere.
    fn rss_bytes() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }

    fn memory_json(samples: &[Sample]) -> String {
        let rows = samples
            .iter()
            .map(|s| {
                let rss = s.rss_bytes.map_or("null".to_string(), |v| v.to_string());
                format!(
                    "{{\"at\":\"{}\",\"tick\":{},\"rss_bytes\":{rss},\"allocations\":{},\"live_bytes\":{}}}",
                    s.label, s.tick, s.allocations, s.live_bytes
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let delta = |a: Option<&Sample>, b: Option<&Sample>| match (a, b) {
            (Some(a), Some(b)) => {
                let rss = match (a.rss_bytes, b.rss_bytes) {
                    (Some(x), Some(y)) => (y as i64 - x as i64).to_string(),
                    _ => "null".into(),
                };
                format!(
                    "{{\"rss_bytes\":{rss},\"allocations\":{},\"live_bytes\":{}}}",
                    b.allocations - a.allocations,
                    b.live_bytes as i64 - a.live_bytes as i64
                )
            }
            _ => "null".into(),
        };
        // `growth` includes start-up sizing (warmup); `steady_growth` runs from the end of warmup.
        let growth = delta(samples.first(), samples.last());
        let steady = delta(samples.iter().find(|s| s.label == "warm"), samples.last());
        format!("{{\"samples\":[{rows}],\"growth\":{growth},\"steady_growth\":{steady}}}")
    }
}
