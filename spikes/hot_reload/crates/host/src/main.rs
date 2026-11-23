//! Headless editor-host stand-in (no window). See README.md for the modes and what is measured.
//!
//! host [--mode salted|unsalted|wrapper] [--edit layout|oneline] [--sim inproc|persistent]
//!      [--gens N] [--csv FILE]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use spike_regia::bevy_ecs::reflect::AppTypeRegistry;
use spike_regia::bevy_reflect::{ReflectMut, ReflectRef};
use spike_regia::regia_app::App;
use spike_regia::hot::{self, GamePlugin, GameRegistrar, Owned, Snapshot};
use spike_regia::loader::{self, serde_json};
use spike_regia::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Salted,
    Unsalted,
    Wrapper,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Variant {
    A,
    B,
    /// A with `speed: f64` instead of `f32`: same field names, different layout.
    C,
}

struct Args {
    mode: Mode,
    oneline: bool,
    retype: bool,
    persistent_sim: bool,
    gens: u32,
    csv: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        mode: Mode::Salted,
        oneline: false,
        retype: false,
        persistent_sim: false,
        gens: 20,
        csv: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mode" => {
                a.mode = match it.next().as_deref() {
                    Some("unsalted") => Mode::Unsalted,
                    Some("wrapper") => Mode::Wrapper,
                    _ => Mode::Salted,
                }
            }
            "--edit" => {
                let e = it.next();
                a.oneline = e.as_deref() == Some("oneline");
                a.retype = e.as_deref() == Some("retype");
            }
            "--sim" => a.persistent_sim = it.next().as_deref() == Some("persistent"),
            "--gens" => a.gens = it.next().and_then(|s| s.parse().ok()).unwrap_or(20),
            "--csv" => a.csv = it.next().map(PathBuf::from),
            other => eprintln!("ignoring argument {other}"),
        }
    }
    a
}

fn generated_source(variant: Variant, gen_no: u32) -> String {
    let header = format!(
        "// Written by the spike host before each rebuild (variant {variant:?}). Do not edit by hand.\n\
         use spike_regia::hot::GameRegistrar;\nuse spike_regia::prelude::*;\n\n\
         /// Generation number of this build.\npub const GEN: u32 = {gen_no};\n"
    );
    let body = match variant {
        Variant::A | Variant::C => {
            r#"/// Name of this variant's movement system.
pub const SYSTEM_NAME: &str = "move_a";

/// Variant A: `legacy` exists, `heading` does not.
#[derive(Component, Reflect, Debug)]
#[reflect(Component, Default)]
pub struct Mover {
    /// Preserved across all generations.
    pub speed: f32,
    /// Removed in variant B.
    pub legacy: u32,
    /// Preserved; incremented by the movement system.
    pub ticks: u64,
}

impl Default for Mover {
    fn default() -> Self {
        Self { speed: 1.0, legacy: 7, ticks: 0 }
    }
}

/// Variant A movement system.
pub fn move_a(mut q: Query<&mut Mover>, mut score: ResMut<crate::Score>) {
    for mut m in &mut q {
        m.ticks += 1;
    }
    score.gen_seen = GEN;
}

/// Registers this variant's types and systems.
pub fn register(reg: &mut GameRegistrar<'_>) {
    reg.component::<Mover>();
    reg.add_systems(Update, move_a);
}
"#
        }
        Variant::B => {
            r#"/// Name of this variant's movement system.
pub const SYSTEM_NAME: &str = "move_b_renamed";

/// Variant B: `legacy` removed, `heading` added (layout grows from 16 to 24 bytes).
#[derive(Component, Reflect, Debug)]
#[reflect(Component, Default)]
pub struct Mover {
    /// Preserved across all generations.
    pub speed: f32,
    /// Preserved; incremented by the movement system.
    pub ticks: u64,
    /// New in variant B.
    pub heading: [f32; 3],
}

impl Default for Mover {
    fn default() -> Self {
        Self { speed: 1.0, ticks: 0, heading: [0.0, 0.0, -1.0] }
    }
}

/// Variant B movement system (renamed).
pub fn move_b_renamed(mut q: Query<&mut Mover>, mut score: ResMut<crate::Score>) {
    for mut m in &mut q {
        m.ticks += 1;
    }
    score.gen_seen = GEN;
}

/// Registers this variant's types and systems.
pub fn register(reg: &mut GameRegistrar<'_>) {
    reg.component::<Mover>();
    reg.add_systems(Update, move_b_renamed);
}
"#
        }
    };
    if variant == Variant::C {
        let body = body.replace("pub speed: f32", "pub speed: f64").replace(
            "Variant A: `legacy` exists",
            "Variant C (A with `speed: f64`): `legacy` exists",
        );
        return format!("{header}{body}");
    }
    format!("{header}{body}")
}

struct Paths {
    root: PathBuf,
    generated: PathBuf,
    hot: PathBuf,
}

fn find_root() -> PathBuf {
    if let Ok(p) = std::env::var("SPIKE_ROOT") {
        return PathBuf::from(p);
    }
    let mut dir = std::env::current_dir().unwrap_or_default();
    loop {
        if dir.join("crates/game/src/generated.rs").exists() {
            return dir;
        }
        if !dir.pop() {
            return PathBuf::from(".");
        }
    }
}

/// Runs cargo, returns (wall ms, built library path).
fn build(paths: &Paths, args: &Args, gen_no: u32) -> Result<(f64, PathBuf), String> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&paths.root);
    let features = if args.persistent_sim {
        "sim-persistent"
    } else {
        "hot-reload"
    };
    match args.mode {
        Mode::Wrapper => {
            cmd.args([
                "build",
                "-p",
                "game_dylib",
                "--lib",
                "--message-format=json-render-diagnostics",
            ]);
            if args.persistent_sim {
                cmd.args(["-F", "sim-persistent"]);
            }
        }
        Mode::Salted | Mode::Unsalted => {
            cmd.args([
                "rustc",
                "-p",
                "game",
                "--lib",
                "--crate-type",
                "dylib",
                "-F",
                features,
                "--message-format=json-render-diagnostics",
            ]);
            if args.mode == Mode::Salted {
                // A new crate disambiguator per generation: new TypeIds, new symbol names.
                cmd.args(["--", "-C", &format!("metadata=hotgen{gen_no}")]);
            }
        }
    }
    let want = if args.mode == Mode::Wrapper {
        "game_dylib"
    } else {
        "game"
    };
    let t = Instant::now();
    let out = cmd.output().map_err(|e| format!("cargo: {e}"))?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if !out.status.success() {
        return Err(format!("build failed:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    let mut lib = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-artifact" || v["target"]["name"] != want {
            continue;
        }
        for f in v["filenames"].as_array().into_iter().flatten() {
            let f = f.as_str().unwrap_or_default();
            if f.ends_with(".so") || f.ends_with(".dll") || f.ends_with(".dylib") {
                lib = Some(PathBuf::from(f));
            }
        }
    }
    lib.map(|l| (ms, l)).ok_or_else(|| "no library artifact".into())
}

/// Size + mtime of the engine dylib the host loaded. If a game build relinks it (a feature or
/// source change in the engine's closure), the running host and the new game disagree: restart.
fn engine_fingerprint(root: &Path) -> Option<(u64, std::time::SystemTime)> {
    let name = if cfg!(windows) {
        "spike_engine_dylib.dll"
    } else {
        "libspike_engine_dylib.so"
    };
    let meta = std::fs::metadata(root.join("target/debug/deps").join(name)).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn registry(world: &World) -> AppTypeRegistry {
    world.resource::<AppTypeRegistry>().clone()
}

fn struct_field<T: Copy + 'static>(value: &dyn PartialReflect, name: &str) -> Option<T> {
    match value.reflect_ref() {
        ReflectRef::Struct(s) => s.field(name)?.try_downcast_ref::<T>().copied(),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct MoverStats {
    count: usize,
    speed_sum: f64,
    min_ticks: u64,
    legacy: Option<u32>,
    heading_z: Option<f32>,
}

fn mover_stats(world: &World) -> MoverStats {
    let reg = registry(world);
    let reg = reg.read();
    let Some(r) = reg.get_with_type_path("game::generated::Mover") else {
        return MoverStats::default();
    };
    let (Some(rc), Some(cid)) = (
        r.data::<ReflectComponent>(),
        world.components().get_id(r.type_id()),
    ) else {
        return MoverStats::default();
    };
    let mut s = MoverStats {
        min_ticks: u64::MAX,
        ..Default::default()
    };
    for a in world.archetypes().iter().filter(|a| a.contains(cid)) {
        for e in a.entities() {
            if let Some(v) = rc.reflect(world.entity(e.id())) {
                let v = v.as_partial_reflect();
                s.count += 1;
                s.speed_sum += struct_field::<f32>(v, "speed")
                    .map(f64::from)
                    .or_else(|| struct_field::<f64>(v, "speed"))
                    .unwrap_or(f64::NAN);
                s.min_ticks = s.min_ticks.min(struct_field::<u64>(v, "ticks").unwrap_or(0));
                s.legacy = struct_field::<u32>(v, "legacy");
                s.heading_z = struct_field::<[f32; 3]>(v, "heading").map(|h| h[2]);
            }
        }
    }
    s
}

/// Sets `speed = i % 100` and `legacy = 42` on every Mover (the "user edits state" step).
fn mutate_movers(world: &mut World) {
    let reg = registry(world);
    let reg = reg.read();
    let Some(r) = reg.get_with_type_path("game::generated::Mover") else {
        return;
    };
    let (Some(rc), Some(cid)) = (
        r.data::<ReflectComponent>(),
        world.components().get_id(r.type_id()),
    ) else {
        return;
    };
    let entities: Vec<Entity> = world
        .archetypes()
        .iter()
        .filter(|a| a.contains(cid))
        .flat_map(|a| a.entities().iter().map(|e| e.id()))
        .collect();
    for (i, e) in entities.into_iter().enumerate() {
        let mut ent = world.entity_mut(e);
        if let Some(mut v) = rc.reflect_mut(&mut ent)
            && let ReflectMut::Struct(s) = v.reflect_mut()
        {
            if let Some(f) = s.field_mut("speed") {
                let _ = f.try_apply(&((i % 100) as f32));
            }
            if let Some(f) = s.field_mut("legacy") {
                let _ = f.try_apply(&42u32);
            }
        }
    }
}

fn resource_u64(world: &World, path: &str, field: &str) -> Option<u64> {
    let reg = registry(world);
    let reg = reg.read();
    let r = reg.get_with_type_path(path)?;
    let rc = r.data::<ReflectComponent>()?;
    let cid = world.components().get_id(r.type_id())?;
    let e = world.resource_entities().get(cid)?;
    let v = rc.reflect(world.entity(e))?;
    match v.reflect_ref() {
        ReflectRef::Struct(s) => s.field(field)?.try_downcast_ref::<u64>().copied(),
        _ => None,
    }
}

fn resource_u32(world: &World, path: &str, field: &str) -> Option<u32> {
    let reg = registry(world);
    let reg = reg.read();
    let r = reg.get_with_type_path(path)?;
    let rc = r.data::<ReflectComponent>()?;
    let cid = world.components().get_id(r.type_id())?;
    let e = world.resource_entities().get(cid)?;
    let v = rc.reflect(world.entity(e))?;
    match v.reflect_ref() {
        ReflectRef::Struct(s) => s.field(field)?.try_downcast_ref::<u32>().copied(),
        _ => None,
    }
}

fn update_system_names(world: &World) -> Vec<String> {
    world
        .resource::<Schedules>()
        .get(Update)
        .and_then(|s| s.systems().ok())
        .map(|it| it.map(|(_, s)| s.name().to_string()).collect())
        .unwrap_or_default()
}

fn mover_type_id(world: &World) -> String {
    let reg = registry(world);
    let reg = reg.read();
    reg.get_with_type_path("game::generated::Mover")
        .map(|r| format!("{:?}", r.type_id()))
        .unwrap_or_default()
}

struct Loaded {
    plugin: Box<dyn GamePlugin>,
    owned: Owned,
}

#[derive(Default, Clone)]
struct Row {
    gen_no: u32,
    variant: String,
    build_ms: f64,
    unload_ms: f64,
    snapshot_ms: f64,
    teardown_ms: f64,
    load_ms: f64,
    register_ms: f64,
    restore_ms: f64,
    onload_ms: f64,
    swap_ms: f64,
    first_frame_ms: f64,
    rss_mb: f64,
    snapshot_kb: f64,
    sim_tick_at_unload: u64,
    sim_tick_after: u64,
    note: String,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn variant_for(args: &Args, gen_no: u32) -> Variant {
    if args.oneline || gen_no % 2 == 1 {
        Variant::A
    } else if args.retype {
        Variant::C
    } else {
        Variant::B
    }
}

fn run() -> Result<(), String> {
    let args = parse_args();
    let root = find_root();
    let paths = Paths {
        generated: root.join("crates/game/src/generated.rs"),
        hot: root.join(".hot"),
        root,
    };
    let original = std::fs::read_to_string(&paths.generated).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&paths.hot);
    println!(
        "host: mode={:?} edit={} sim={} gens={} os={} abi={:016x}",
        args.mode,
        if args.oneline { "oneline" } else { "layout" },
        if args.persistent_sim {
            "persistent"
        } else {
            "inproc"
        },
        args.gens,
        std::env::consts::OS,
        hot::ABI_HASH
    );

    let engine = engine_fingerprint(&paths.root);
    let mut app = App::new();
    app.init_resource::<AppTypeRegistry>();
    app.init_resource::<PersistentStore>();
    let rss0 = loader::rss_bytes() as f64 / 1e6;
    let mut rows: Vec<Row> = Vec::new();
    let mut current: Option<Loaded> = None;
    let mut prev_variant = Variant::A;
    let mut prev_legacy = None;
    let mut prev_heading = None;
    let mut failure = None;

    for gen_no in 1..=args.gens {
        let variant = variant_for(&args, gen_no);
        std::fs::write(&paths.generated, generated_source(variant, gen_no)).map_err(|e| e.to_string())?;
        let (build_ms, built) = build(&paths, &args, gen_no)?;
        if engine_fingerprint(&paths.root) != engine {
            return Err("the game build relinked the engine dylib (engine source or feature set changed): restart the editor".into());
        }
        let mut row = Row {
            gen_no,
            variant: format!("{variant:?}"),
            build_ms,
            ..Default::default()
        };
        let world_before = current.is_some();
        let swap = Instant::now();
        let mut snapshot = Snapshot::default();
        if let Some(old) = current.take() {
            row.sim_tick_at_unload = resource_u64(app.world(), "game::SimView", "tick").unwrap_or(0);
            let t = Instant::now();
            old.plugin.on_unload(app.world_mut());
            row.unload_ms = ms(t);
            row.sim_tick_at_unload =
                resource_u64(app.world(), "game::SimView", "tick").unwrap_or(row.sim_tick_at_unload);
            let t = Instant::now();
            snapshot = hot::take_snapshot(app.world(), &old.owned);
            row.snapshot_ms = ms(t);
            row.snapshot_kb = snapshot.bytes() as f64 / 1024.0;
            let t = Instant::now();
            let td = hot::teardown(app.world_mut(), &old.owned);
            row.teardown_ms = ms(t);
            if td.leftover_instances != 0 {
                return Err(format!("teardown left {} instances", td.leftover_instances));
            }
            drop(old.plugin); // after this, no pointer into the old library may remain
        }
        let t = Instant::now();
        let copy = loader::copy_unique(&built, &paths.hot, gen_no).map_err(|e| e.to_string())?;
        let plugin = loader::load_game(&copy).map_err(|e| e.to_string())?;
        row.load_ms = ms(t);
        let t = Instant::now();
        let mut reg = GameRegistrar::new(&mut app, gen_no);
        plugin.register(&mut reg);
        let owned = reg.finish();
        row.register_ms = ms(t);
        // The hazard check: a TypeId bevy_ecs has seen before keeps its old ComponentId + layout.
        let clashes = hot::check_component_identity(app.world(), &owned);
        let unsound: Vec<_> = clashes.iter().filter(|c| c.registered != c.new).collect();
        if !clashes.is_empty() {
            row.note = format!("{} types reuse an old ComponentId", clashes.len());
        }
        if !unsound.is_empty() {
            let msg = unsound
                .iter()
                .map(|c| {
                    format!(
                        "{} (registered {} B, new {} B)",
                        c.type_path,
                        c.registered.size(),
                        c.new.size()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            failure = Some(format!(
                "gen {gen_no}: UNSOUND — TypeId reused with a different layout: {msg}. bevy_ecs would store the new \
                 type in the old column layout. Reload refused."
            ));
            row.note = "refused: layout clash".into();
            rows.push(row);
            break;
        }
        let t = Instant::now();
        let rep = hot::restore(app.world_mut(), &snapshot);
        row.restore_ms = ms(t);
        let t = Instant::now();
        plugin.on_load(app.world_mut());
        row.onload_ms = ms(t);
        row.swap_ms = ms(swap);
        if !app.world().contains_resource::<AppTypeRegistry>() {
            return Err("registry lost".into());
        }
        let t = Instant::now();
        app.update();
        row.first_frame_ms = ms(t);
        if !world_before {
            mutate_movers(app.world_mut());
        }
        app.update();
        app.update();
        row.sim_tick_after = resource_u64(app.world(), "game::SimView", "tick").unwrap_or(0);

        // --- verification -------------------------------------------------------------------
        let stats = mover_stats(app.world());
        let gen_seen = resource_u32(app.world(), "game::Score", "gen_seen").unwrap_or(0);
        let names = update_system_names(app.world());
        let expect_sys = if variant == Variant::B {
            "move_b_renamed"
        } else {
            "move_a"
        };
        let other_sys = if variant == Variant::B {
            "move_a"
        } else {
            "move_b_renamed"
        };
        let mut errors = Vec::new();
        if gen_seen != gen_no {
            errors.push(format!(
                "gen_seen {gen_seen} != {gen_no} (new system did not run)"
            ));
        }
        // System names need bevy_ecs' `debug` feature (off in the engine); without it, check the
        // count: exactly the game's 3 Update systems (old ones removed, renamed one present).
        let named = names.iter().any(|n| n.contains("move_"));
        if (named
            && (!names.iter().any(|n| n.ends_with(expect_sys))
                || names.iter().any(|n| n.ends_with(other_sys))))
            || names.len() != 3
        {
            errors.push(format!("Update systems wrong: {names:?}"));
        }
        if stats.count != game_entities() {
            errors.push(format!("{} movers, expected {}", stats.count, game_entities()));
        }
        let expect_speed: f64 = (0..game_entities()).map(|i| (i % 100) as f64).sum();
        if (stats.speed_sum - expect_speed).abs() > 0.5 {
            errors.push(format!(
                "speed sum {} != {expect_speed} (state lost)",
                stats.speed_sum
            ));
        }
        if stats.min_ticks < u64::from(gen_no) * 3 {
            errors.push(format!(
                "min ticks {} < {} (ticks not preserved)",
                stats.min_ticks,
                gen_no * 3
            ));
        }
        match variant {
            Variant::A | Variant::C => {
                let want = if prev_variant != Variant::B {
                    prev_legacy.or(Some(42))
                } else {
                    Some(7)
                };
                if stats.legacy != want {
                    errors.push(format!("legacy {:?} != {want:?}", stats.legacy));
                }
            }
            Variant::B => {
                let want = if prev_variant == Variant::B {
                    prev_heading
                } else {
                    Some(-1.0)
                };
                if stats.heading_z != want || stats.legacy.is_some() {
                    errors.push(format!(
                        "heading {:?} != {want:?} or legacy present",
                        stats.heading_z
                    ));
                }
            }
        }
        if world_before && !args.persistent_sim && row.sim_tick_after < row.sim_tick_at_unload {
            errors.push(format!(
                "sim restarted behind its snapshot ({} < {})",
                row.sim_tick_after, row.sim_tick_at_unload
            ));
        }
        prev_variant = variant;
        prev_legacy = stats.legacy;
        prev_heading = stats.heading_z;
        row.rss_mb = loader::rss_bytes() as f64 / 1e6;
        println!(
            "gen {:>2} {:?} build {:>7.0} ms | swap {:>6.1} ms (unload {:.1}, snap {:.1}, teardown {:.1}, load {:.1}, register {:.1}, restore {:.1}, on_load {:.1}) | 1st frame {:.1} ms | rss {:.1} MB | restored {} comps/{} res, defaulted {}, dropped {} | sim tick {}→{} | Mover {} | {}",
            gen_no,
            variant,
            row.build_ms,
            row.swap_ms,
            row.unload_ms,
            row.snapshot_ms,
            row.teardown_ms,
            row.load_ms,
            row.register_ms,
            row.restore_ms,
            row.onload_ms,
            row.first_frame_ms,
            row.rss_mb,
            rep.components,
            rep.resources,
            rep.defaulted_fields,
            rep.dropped_fields,
            row.sim_tick_at_unload,
            row.sim_tick_after,
            mover_type_id(app.world()),
            row.note
        );
        rows.push(row);
        current = Some(Loaded { plugin, owned });
        if !errors.is_empty() {
            failure = Some(format!("gen {gen_no}: {}", errors.join("; ")));
            break;
        }
    }

    // Final unload (joins the sim thread in option (a)).
    if let Some(old) = current.take() {
        old.plugin.on_unload(app.world_mut());
    }
    let _ = std::fs::write(&paths.generated, original);

    summarize(&rows, rss0);
    if let Some(csv) = &args.csv {
        write_csv(csv, &rows).map_err(|e| e.to_string())?;
    }
    match failure {
        Some(f) => Err(f),
        None => {
            println!("RESULT: OK — {} loads, all checks passed", rows.len());
            Ok(())
        }
    }
}

fn game_entities() -> usize {
    5_000
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn summarize(rows: &[Row], rss0: f64) {
    let reloads: Vec<&Row> = rows.iter().skip(1).collect();
    if reloads.is_empty() {
        return;
    }
    let col = |f: fn(&Row) -> f64| median(reloads.iter().map(|r| f(r)).collect());
    let first_rss = rows.first().map_or(0.0, |r| r.rss_mb);
    let last_rss = rows.last().map_or(0.0, |r| r.rss_mb);
    println!(
        "SUMMARY reloads={} median: build {:.0} ms, swap {:.1} ms (unload {:.1}, snapshot {:.1}, teardown {:.1}, load {:.1}, register {:.1}, restore {:.1}, on_load {:.1}), first frame {:.1} ms; max build {:.0} ms; rss start {:.1} MB, after gen1 {:.1} MB, end {:.1} MB ({:+.2} MB/reload)",
        reloads.len(),
        col(|r| r.build_ms),
        col(|r| r.swap_ms),
        col(|r| r.unload_ms),
        col(|r| r.snapshot_ms),
        col(|r| r.teardown_ms),
        col(|r| r.load_ms),
        col(|r| r.register_ms),
        col(|r| r.restore_ms),
        col(|r| r.onload_ms),
        col(|r| r.first_frame_ms),
        reloads.iter().map(|r| r.build_ms).fold(0.0, f64::max),
        rss0,
        first_rss,
        last_rss,
        (last_rss - first_rss) / reloads.len() as f64
    );
}

fn write_csv(path: &Path, rows: &[Row]) -> std::io::Result<()> {
    let mut s = String::from(
        "gen,variant,build_ms,unload_ms,snapshot_ms,teardown_ms,load_ms,register_ms,restore_ms,onload_ms,swap_ms,first_frame_ms,rss_mb,snapshot_kb,sim_tick_at_unload,sim_tick_after,note\n",
    );
    for r in rows {
        s.push_str(&format!(
            "{},{},{:.1},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.1},{:.1},{},{},{}\n",
            r.gen_no,
            r.variant,
            r.build_ms,
            r.unload_ms,
            r.snapshot_ms,
            r.teardown_ms,
            r.load_ms,
            r.register_ms,
            r.restore_ms,
            r.onload_ms,
            r.swap_ms,
            r.first_frame_ms,
            r.rss_mb,
            r.snapshot_kb,
            r.sim_tick_at_unload,
            r.sim_tick_after,
            r.note
        ));
    }
    std::fs::write(path, s)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("FAILED: {e}");
        std::process::exit(2);
    }
}
