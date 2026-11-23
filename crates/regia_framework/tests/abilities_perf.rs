// SPDX-License-Identifier: MPL-2.0
//! Microbenchmarks for spec 53 § 10 (`docs/06-testing-and-ci.md` § 9): one `AbilitySet` step at the
//! budget's size. Every test is `#[ignore]`: the gate never runs them, and they assert only that the
//! work happened, never a time. Budgets are for the min-spec machine and are judged by the lead from
//! the printed numbers. Run with
//!
//! ```text
//! cargo test -p regia_framework --release --test abilities_perf -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_framework::abilities::*;

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

fn measure(name: &str, iterations: usize, app: &mut App) {
    for _ in 0..50 {
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().clear_trackers();
    }
    let samples = (0..iterations)
        .map(|_| {
            let start = Instant::now();
            app.world_mut().run_schedule(FixedUpdate);
            let spent = start.elapsed();
            app.world_mut().clear_trackers();
            spent
        })
        .collect();
    report(name, samples);
}

fn modifier(attribute: &str, op: ModOp, amount: f32) -> Modifier {
    Modifier {
        attribute: attribute.to_owned(),
        op,
        amount: Magnitude::Const(amount),
        per_stack: false,
        when: None,
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AbilityPlugin::default());
    app.update();
    app
}

fn attributes() -> Attributes {
    Attributes::from_list([
        Attribute::new("health", 100.0, 0.0, 9999.0),
        Attribute::new("stamina", 50.0, 0.0, 100.0),
        Attribute::new("strength", 10.0, 0.0, 100.0),
        Attribute::new("speed", 4.0, 0.0, 12.0),
    ])
    .expect("attributes")
}

/// § 10: `AbilitySet` total <= 0.5 ms for 1,000 entities x 8 effects.
#[test]
#[ignore = "hand-run microbenchmark"]
fn step_1000_entities_8_effects() {
    let mut app = app();
    for index in 0..8 {
        let mut asset = EffectAsset::new(Lifetime::UntilRemoved);
        asset.live_mods = vec![
            modifier("strength", ModOp::AddPre, 1.0),
            modifier("speed", ModOp::Scale, 0.9),
        ];
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_effect(&format!("e{index}"), asset);
    }
    for _ in 0..1_000 {
        let target = app.world_mut().spawn(attributes()).id();
        for index in 0..8 {
            apply_effect(app.world_mut(), ApplyEffect::new(&format!("e{index}"), target)).expect("apply");
        }
    }
    measure("abilities_step_1000x8", 500, &mut app);
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.live_effects, 8_000);
    // Untimed fixed effects: the dirty flag skips every entity once settled (ADR-2012).
    assert_eq!(stats.entities_skipped, 1_000);
}

/// The same with timed effects (a 1,000 s lifetime, so none ends during the run): each clock is
/// written every step, and a write of clocks alone does not end the dirty flag's skip (ADR-2012).
#[test]
#[ignore = "hand-run microbenchmark"]
fn step_1000_entities_8_timed_effects() {
    let mut app = app();
    for index in 0..8 {
        let mut asset = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1_000.0)));
        asset.live_mods = vec![
            modifier("strength", ModOp::AddPre, 1.0),
            modifier("speed", ModOp::Scale, 0.9),
        ];
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_effect(&format!("e{index}"), asset);
    }
    for _ in 0..1_000 {
        let target = app.world_mut().spawn(attributes()).id();
        for index in 0..8 {
            apply_effect(app.world_mut(), ApplyEffect::new(&format!("e{index}"), target)).expect("apply");
        }
    }
    measure("abilities_step_1000x8_timed", 500, &mut app);
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.live_effects, 8_000);
    assert_eq!(stats.entities_skipped, 1_000);
}

/// § 10: <= 0.1 ms when nothing changed.
#[test]
#[ignore = "hand-run microbenchmark"]
fn step_1000_idle_entities() {
    let mut app = app();
    for _ in 0..1_000 {
        app.world_mut().spawn(attributes());
    }
    measure("abilities_step_1000_idle", 500, &mut app);
    assert_eq!(app.world().resource::<AbilityStats>().entities_skipped, 1_000);
}

/// Wall time inside each `abilities_*` span, summed over the measured steps, with the number of
/// entries: which pass of the 1,000 x 8 step costs what. A subscriber is installed, so the total
/// is above the plain run; read the shares, not the sum.
mod by_pass {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tracing::Subscriber;
    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    /// Summed time and entries per span name.
    pub type Totals = Arc<Mutex<BTreeMap<&'static str, (Duration, u64)>>>;

    struct Entered(Instant);

    struct Timing(Totals);

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Timing {
        fn on_new_span(&self, _: &Attributes<'_>, _: &Id, _: Context<'_, S>) {}

        fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(id)
                && span.name().starts_with("abilities_")
            {
                span.extensions_mut().replace(Entered(Instant::now()));
            }
        }

        fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
            let Some(span) = ctx.span(id) else { return };
            let Some(Entered(at)) = span.extensions_mut().remove::<Entered>() else {
                return;
            };
            if let Ok(mut totals) = self.0.lock() {
                let slot = totals.entry(span.name()).or_default();
                slot.0 += at.elapsed();
                slot.1 += 1;
            }
        }
    }

    /// Runs `f` with the timing layer as the thread's subscriber and prints each span's mean.
    pub fn run(steps: u32, f: impl FnOnce()) {
        let totals = Totals::default();
        let subscriber = tracing_subscriber::registry().with(Timing(totals.clone()));
        tracing::subscriber::with_default(subscriber, f);
        let Ok(totals) = totals.lock() else { return };
        for (name, (spent, entries)) in totals.iter() {
            println!(
                "perf-micro by_pass {name}: entries={entries} mean_per_step={:?}",
                *spent / steps
            );
        }
    }
}

/// The 1,000 x 8 step split by pass (`by_pass`).
#[test]
#[ignore = "hand-run microbenchmark"]
fn step_1000_entities_8_effects_by_pass() {
    let mut app = app();
    for index in 0..8 {
        let mut asset = EffectAsset::new(Lifetime::UntilRemoved);
        asset.live_mods = vec![
            modifier("strength", ModOp::AddPre, 1.0),
            modifier("speed", ModOp::Scale, 0.9),
        ];
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_effect(&format!("e{index}"), asset);
    }
    for _ in 0..1_000 {
        let target = app.world_mut().spawn(attributes()).id();
        for index in 0..8 {
            apply_effect(app.world_mut(), ApplyEffect::new(&format!("e{index}"), target)).expect("apply");
        }
    }
    for _ in 0..50 {
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().clear_trackers();
    }
    by_pass::run(200, || {
        for _ in 0..200 {
            app.world_mut().run_schedule(FixedUpdate);
            app.world_mut().clear_trackers();
        }
    });
    assert_eq!(app.world().resource::<AbilityStats>().live_effects, 8_000);
    // The lookups a quiet step makes, alone: 8,000 of each.
    let world = app.world_mut();
    let entities: Vec<Entity> = world
        .query_filtered::<Entity, With<AppliedEffects>>()
        .iter(world)
        .collect();
    let world = app.world();
    let names: Vec<String> = (0..8).map(|i| format!("e{i}")).collect();
    let map: std::collections::BTreeMap<String, u32> = names.iter().cloned().zip(0..).collect();
    let time = |name: &str, f: &mut dyn FnMut() -> u64| {
        let start = Instant::now();
        let mut seen = 0;
        for _ in 0..100 {
            seen += f();
        }
        println!(
            "perf-micro lookups {name}: {:?} per 8,000 (seen {seen})",
            start.elapsed() / 100
        );
    };
    time("world_get_applied_effects", &mut || {
        let mut n = 0;
        for &e in &entities {
            for _ in 0..8 {
                n += world.get::<AppliedEffects>(e).map_or(0, |l| l.list.len() as u64);
            }
        }
        n
    });
    time("world_get_resource_assets", &mut || {
        let mut n = 0;
        for _ in 0..8_000 {
            n += u64::from(world.get_resource::<AbilityAssets>().is_some());
        }
        n
    });
    time("btreemap_string_8_keys", &mut || {
        let mut n = 0;
        for _ in &entities {
            for name in &names {
                n += u64::from(*std::hint::black_box(&map).get(name.as_str()).unwrap_or(&0));
            }
        }
        n
    });
    time("list_get_by_id", &mut || {
        let mut n = 0;
        for &e in &entities {
            if let Some(list) = world.get::<AppliedEffects>(e) {
                for entry in &list.list {
                    n += u64::from(list.get(entry.id).is_some());
                }
            }
        }
        n
    });
}
