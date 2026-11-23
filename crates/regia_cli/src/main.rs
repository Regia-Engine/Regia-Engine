// SPDX-License-Identifier: MPL-2.0
//! The `regia` command: new, editor, run, cook, package, prepare, doctor.
//!
//! Specification: `docs/specs/21-tooling-and-packaging.md`. `regia package` builds Linux, Windows
//! and Android (APK / AAB, release signing) packages; `regia install-android` installs and
//! launches the Android build.

// Before `commands`: its `outln!`/`out!` macros print for the `--json` mode (ADR-7217).
#[macro_use]
mod json_out;
mod android;
mod commands;
mod manifest;
#[cfg(all(windows, test))]
mod owner_only;
mod process;

use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Regia engine command-line tool.
#[derive(Debug, Parser)]
#[command(name = "regia", version, about)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Call one protocol method and print its result (specs/21 § 1: `regia agent call <method> [json]`).
    Call(commands::agent::AgentCallArgs),
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Talk to a running Regia Engine app through the agent protocol (specs/23).
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Build a distributable package (Linux tar.gz, Windows zip, Android APK/AAB with release signing);
    /// `--server` builds the dedicated server (spec 76).
    Package(commands::package::PackageArgs),
    /// Package, install with adb, launch, and stream the app's logcat.
    InstallAndroid(commands::install_android::InstallAndroidArgs),
    /// Steam packaging and upload (specs/24 § 3): `--steam` with `regia package`,
    /// `regia steam upload`, `regia steam input-manifest` (the Steam Input actions file) and
    /// `regia steam layout` (the controller layout and its check).
    #[command(subcommand)]
    Steam(commands::steam::SteamCommand),
    /// Lockstep diagnosis: `regia net desync-replay <bundle>` replays a `*.desync.ron` bundle
    /// headless and names the first diverging tick and subsystem (spec 120 § 7.6);
    /// `regia net replay-audit <file>` checks a `.lsreplay` match replay bit for bit (ADR-8513).
    #[command(subcommand)]
    Net(commands::net::NetCommand),
    /// Run an MCP stdio server that attaches to a running Regia Engine app (specs/23 § 2).
    Mcp(commands::mcp::McpArgs),
    /// Cook project assets for the host platform (specs/12 § 6).
    Cook(commands::cook::CookArgs),
    /// Run content-format migrators (specs/12 § 4a).
    UpgradeProject(commands::upgrade_project::UpgradeProjectArgs),
    /// Preview a cooked asset in a window.
    Preview(commands::preview::PreviewArgs),
    /// Build and run the project's desktop launcher; `--dev` adds live asset reload (specs/12 § 8).
    Run(commands::run::RunArgs),
    /// Build and run the game's dedicated server from the dev cache, with `--clients N` headless
    /// clients and `--agent` the agent protocol server (spec 76 § 10).
    Server(commands::server::ServerArgs),
    /// One image of a running app's view: the real-time frame, or `--reference` traced by the
    /// reference path tracer (specs/27 § 5).
    Render(commands::render::RenderArgs),
    /// Bake a diffuse GI probe volume (`.fgiv`) with the reference path tracer: a built-in
    /// scene (`cornell`, `furnace`, `rolling-hills`) or a bake description (`.ron`). Boxed: its
    /// arguments are several times any other command's (clippy `large_enum_variant`).
    BakeGi(Box<commands::bake_gi::BakeGiArgs>),
    /// Run the public benchmark scene set headless and print frame stats; `--fetch` downloads it
    /// with checksums and licences (ADR-2089).
    Bench(commands::bench::BenchArgs),
    /// Remove regenerable project state: the cook and asset-db caches by default, `--cache` adds shader
    /// and pipeline caches, `--all` adds `build/`, `dist/` and `target/` (`--dry-run` lists first).
    Clean(commands::clean::CleanArgs),
    /// Check toolchains and print fixes.
    Doctor(commands::doctor::DoctorArgs),
    /// Warm shipped pipelines headless and write a stamp (stdout is JSON lines).
    Prepare(commands::prepare::PrepareArgs),
    /// Create a project from a template (`blank`, `first_person`, `third_person`, `top_down`, `quick`, `net-game`).
    New(commands::new_project::NewArgs),
    /// Bake a lightmap atlas (`.flmap`) for a scene's static meshes on the CPU: a built-in scene
    /// (`cornell`, `furnace`, `rolling-hills`) or a bake description (`.ron`); `--inspect` reads
    /// an existing `.flmap`.
    BakeLightmaps(commands::bake_lightmaps::BakeLightmapsArgs),
    /// What a built APK or AAB is made of: sizes per native ABI, paks, assets, dex, resources;
    /// `--json` for tools, `--budget-mb` to fail over a size.
    ApkSize(commands::apk_size::ApkSizeArgs),
    /// Data tables without the game's row type: `validate`, `to-csv`, `to-ron` (RFC 4180 CSV and
    /// `.table.ron`).
    #[command(subcommand)]
    Table(commands::table::TableCommand),
    /// Rule sets (`.rules.ron`, a match mode as data): `rules check <file>...` parses, checks and
    /// resolves each file and prints every error with its code and field path.
    #[command(subcommand)]
    Rules(commands::rules::RulesCommand),
    /// Terrain tools: `terrain passages import <tunnels.json>` converts DO27's `do27-tunnels/1` passage
    /// description to `<map>.passages.ron`; `terrain passages export` writes it back (spec 121 § 9.1);
    /// `terrain cook <map.meshlayers.ron>` cooks the canonical and render halves (spec 121 § 9.2);
    /// `terrain check-layers <map>` runs the spec 121 § 10 checks (`terrain.bake_report`).
    #[command(subcommand)]
    Terrain(commands::terrain::TerrainCommand),
    /// The DCC scene feed's reference client: stream a glTF file, record, replay, dump (specs/60 § 5.1).
    #[command(subcommand)]
    DccFeed(commands::dcc_feed::DccFeedCommand),
    /// Procedural graphs: `procgen graph check <file>` compiles a graph or variant and prints every error;
    /// `procgen import-scatter <file.scatter.ron> --out <dir>` writes a `.biome.ron`.
    #[command(subcommand)]
    Procgen(commands::procgen::ProcgenCommand),
    /// Query the cook's asset registry without loading assets: `assets query` by tag, type,
    /// path, dependencies or referencers; `assets tags` lists tag keys and values.
    #[command(subcommand)]
    Assets(commands::assets_query::AssetsCommand),
    /// Content chunks: cook a DLC or patch chunk against a frozen base (specs/40).
    #[command(subcommand)]
    Content(commands::content::ContentCommand),
    /// Localization workflow: `gather` keys into each locale, `validate` the locales, write a
    /// `pseudo` locale, print `plural` categories (exit 1 when a check fails).
    #[command(subcommand)]
    Loc(commands::loc::LocCommand),
    /// Unwrap and pack a `.obj` mesh into a lightmap UV atlas (charts, conformal flattening,
    /// bitmap packing; ADR-1483); `--out` writes the mesh with the atlas UVs, `--json` for tools.
    Atlas(commands::atlas::AtlasArgs),
    /// Video tools: `media encode <frames dir | file.y4m> [file.wav] -o out.webm` encodes AV1 and
    /// Opus in WebM with Regia's own encoder and checks the file with the cook's validator (ADR-3369).
    #[command(subcommand)]
    Media(commands::media_encode::MediaCommand),
    /// Import a Tiled tileset's (or map's) Wang sets and terrains as `.autotile.ron` rules, one
    /// file per tileset with usable terrains (ADR-2597).
    TiledAutotile(commands::tiled_autotile::TiledAutotileArgs),
    /// Inspect an `.fbx` (ADR-1602) or `.obj` file with its importer: nodes, meshes, skins, morph
    /// targets, materials and texture lookups, clips, unit and up axis; `--out` writes the
    /// converted `.glb` the cook would import, `--json` for tools.
    Import(commands::import::ImportArgs),
    /// Export a level, a prefab or some entities to a DCC tool: `export glb --level L | --prefab P
    /// [--entities GUIDS] --out FILE.glb` (nodes, meshes, PBR materials with PNG textures, lights,
    /// cameras; skins and clips with `--skins` and `--animations`).
    /// `export usda --glb FILE.glb --out FILE.usda` writes a USDA stage from that binary (ADR-4604).
    #[command(subcommand)]
    Export(commands::export::ExportCommand),
    /// Pre-fracture a closed mesh (`.glb`, or `--box X,Y,Z`) into Voronoi chunks and cook the
    /// destruction chunk asset: writes `<out>.fracture.ron` and `<out>.destructible.ron`.
    Fracture(commands::fracture::FractureArgs),
    /// The project's extensions: `plugin list`, `info`, `check`, `enable`, `disable`, `sync`,
    /// `build`, `new` (specs/57). Extension code is trusted native code, not sandboxed.
    #[command(subcommand)]
    Plugin(commands::plugin::PluginCommand),
    /// Convert a level between the single file (`.level.ron`) and the directory form
    /// (`.level.d/`, one file per entity): `level split`, `level merge` (spec 37).
    #[command(subcommand)]
    Level(commands::level::LevelCommand),
    /// Trace files: `trace export` converts a `.regiatrace` recording to JSON lines or the
    /// public Trace Event Format.
    #[command(subcommand)]
    Trace(commands::trace::TraceCommand),
    /// Run the functional tests placed in a level (`regia test --level L`), or launch a
    /// multi-process playthrough (`regia test session FILE`). The desktop game still opens its
    /// window (`REGIA_SESSION_RENDER=0` is advisory, spec 41 § 2.8). Exit 0 passed, 1 failed, 2
    /// aborted or undecided, 3 no tests ran, 4 harness or launch failure (specs/41).
    Test(commands::test::TestArgs),
    /// Engine releases and the version policy (spec 94): `engine info` (this engine, a project's
    /// requirement and pin), `engine package` (a release directory with a file manifest), `engine
    /// verify` (check a release copy), `engine chunk` (release chunks under a byte cap, ADR-8404);
    /// `--json` for tools.
    #[command(subcommand)]
    Engine(commands::engine::EngineCommand),
    /// Run the editor on a project; `--headless` runs it without a window for CI and cloud agents
    /// (one `{"event":"ready"}` JSON line on stdout when the agent server listens; specs/21 § 1).
    Editor(commands::editor::EditorArgs),
    /// Generators (`generators/<name>.toml`, specs/23 § 6): `gen list`, `gen run <name>` (outputs
    /// with provenance generator + seed), `gen verify` (exit 1 with a diff list when a byte differs).
    #[command(subcommand)]
    Gen(commands::r#gen::GenCommand),
    /// Product viewer packages (ADR-2540): `viewer pack <kit> <recipe> -o <dir>` cooks one with a
    /// size report, `viewer verify <dir>` checks its hashes, `viewer standin -o <dir>` writes the
    /// generic stand-in kit and recipes.
    #[command(subcommand)]
    Viewer(commands::viewer::ViewerCommand),
    /// Write the project map (`project-map.md` and `project-map.json`): kinds, prefabs, components,
    /// the systems that read and write them (`--live`), levels and a where-to-look index (spec 101).
    Describe(commands::describe::DescribeArgs),
    /// Project-side checks before a merge or a cook: the kind lint at `[kinds] strictness`; exit 1 on
    /// an error finding (spec 101 § 5).
    Preflight(commands::describe::PreflightArgs),
    /// Golden proposals agents made with `agent.bless_golden`: `golden list`, `golden accept <id>`
    /// (a person at a terminal, typing the golden's name), `golden reject <id>` (ADR-2821).
    #[command(subcommand)]
    Golden(commands::golden::GoldenCommand),
    /// Bake the open level's local reflection captures in the running app and save them to a
    /// `.fcap` named in the level; `--check FILE` reads a `.fcap` offline (ADR-2860).
    BakeCaptures(commands::bake_captures::BakeCapturesArgs),
    /// The git pre-commit hook that runs `assets validate --staged`: `hooks install` (`--force`
    /// replaces another hook and keeps it), `hooks uninstall`, `hooks status` (ADR-7789).
    #[command(subcommand)]
    Hooks(commands::hooks::HooksCommand),
    /// Replay a key track, check that it parses, or install a package and drive physical keys
    /// (ADR-8403, ADR-8452).
    /// `play-gate replay TRACK [--log FILE] [--require-clean-log] [--json]`,
    /// `play-gate check TRACK [--json]`,
    /// `play-gate run ARCHIVE TRACK --fresh-home --out DIR [--keymap FILE] [--within-frames N] [--json]`.
    #[command(subcommand)]
    PlayGate(commands::play_gate::PlayGateCommand),
    /// Stitch split tester uploads and summarize sessions, event rates and storms (ADR-8035).
    #[command(subcommand)]
    Logs(commands::logs::LogsCommand),
    /// Build a labeled JPEG contact sheet and a luma CSV.
    #[command(after_help = "\
Examples:
    regia contact-sheet --frames a.png b.png --out sheet.jpg
    regia contact-sheet --dirs frames/ --out sheet.jpg
    regia contact-sheet --pairs before/ after/ --out sheet.jpg

Exit codes:
    0  wrote the JPEG and the CSV beside it
    2  usage, a missing file, or an unsupported image (contact_sheet_usage, contact_sheet_io, contact_sheet_format)
")]
    ContactSheet(commands::contact_sheet::ContactSheetArgs),
    /// Verify, diff, or plan the upload of a release directory (ADR-8557).
    #[command(subcommand)]
    Release(commands::release::ReleaseCommand),
}

/// True for `regia package --server --platform macos`. macOS is no package platform (ADR-264), so
/// clap would refuse the value with its own text; a server package gets the stable code of spec 76
/// § 3 instead.
fn server_package_on_macos(args: &[String]) -> bool {
    let has = |text: &str| args.iter().any(|a| a == text);
    args.get(1).is_some_and(|c| c == "package")
        && has("--server")
        && (has("--platform=macos") || args.windows(2).any(|w| w[0] == "--platform" && w[1] == "macos"))
}

fn main() -> ExitCode {
    // Through `brand::env_var`, so the variable's name from before the rename still works, and
    // `brand::current_log_filter`, so directives naming the old crate names still match (ADR-1388,
    // ADR-1421, ADR-1429).
    let filter = regia_core::brand::env_var(regia_core::log::FILTER_ENV)
        .ok()
        .and_then(|text| EnvFilter::try_new(regia_core::brand::current_log_filter(&text)).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    if server_package_on_macos(&std::env::args().collect::<Vec<_>>()) {
        eprintln!("error: {}", commands::package::ServerPlatformUnsupported("macos"));
        return ExitCode::from(2);
    }
    // `package` is a flat command that requires `--platform`. `package parity`
    // is intercepted here so that flag is not demanded (ADR-8428).
    let parity_argv: Vec<String> = std::env::args().skip(1).collect();
    if parity_argv.first().is_some_and(|word| word == "package")
        && parity_argv.get(1).is_some_and(|word| word == "parity")
    {
        return commands::package_parity::run(&parity_argv[2..]);
    }
    // `export usda` is attached here, not on `ExportCommand`, so `commands/export.rs`
    // can change beside this file (ADR-4604). Help for every command, including
    // `export`, comes from this command. Anything other than `usda` is parsed again
    // by the derived parser.
    let mut command = Cli::command();
    commands::export_usda::attach(&mut command);
    let matches = command.try_get_matches().unwrap_or_else(|err| err.exit());
    if let Some(code) = commands::export_usda::try_run(&matches) {
        return code;
    }
    let cli = Cli::parse();
    // `prepare` returns 0 (ready), 2 (`--check` and a prepare is still needed) or 1.
    if let CliCommand::Prepare(args) = &cli.command {
        return commands::prepare::run(args);
    }
    // `release` returns 0, 1 (verify found problems) or 2 (an input could not be read).
    if let CliCommand::Release(command) = &cli.command {
        return commands::release::run(command);
    }
    // `bake-gi --from-scene --check` returns 0 (current), 2 (stale) or 1.
    if let CliCommand::BakeGi(args) = &cli.command
        && args.from_scene.check
    {
        return commands::bake_gi::from_scene::check(args);
    }
    // `render --job` returns 0 (done), 2 (refused), 3 (failed), 4 (cancelled) or 1 (ADR-1616).
    if let CliCommand::Render(args) = &cli.command
        && args.job.is_some()
    {
        return commands::render::run_job(args);
    }
    // `net desync-replay` returns 0 (replayed) or 1 (refused or failed; ADR-8455); `net replay-audit`
    // returns 0 (bit-exact) or 1 (a hash differs, or the file was refused; ADR-8513).
    if let CliCommand::Net(command) = &cli.command {
        return commands::net::run(command);
    }
    // `bench` returns 0 (done), 2 (refused) or 1 (ADR-2089).
    if let CliCommand::Bench(args) = &cli.command {
        return commands::bench::run(args);
    }
    // `test` returns 0 (passed), 1 (failed), 2 (aborted or undecided), 3 (no tests) or 4 (harness failure).
    if let CliCommand::Test(args) = &cli.command {
        return commands::test::run(args);
    }
    // `terrain overview-bake` returns 2 on a bad map (ADR-8333). Other terrain errors stay 1.
    if let CliCommand::Terrain(command) = &cli.command {
        return commands::terrain::run_exit(command);
    }
    // `contact-sheet` returns 0 (wrote the files) or 2 (usage, missing file, or bad image).
    if let CliCommand::ContactSheet(args) = &cli.command {
        return commands::contact_sheet::run(args);
    }
    // `package`, `run` and a plain `cook` with `--json` end with one compact object on stdout
    // (spec 21 § 1, ADR-7217); everything else they print goes to stderr.
    if let Some(code) = run_json_command(&cli.command) {
        return code;
    }
    // The `mcp` command needs an async runtime; everything else is sync.
    if matches!(&cli.command, CliCommand::Mcp(_)) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        return match rt.block_on(async {
            match &cli.command {
                CliCommand::Mcp(args) => commands::mcp::run(args).await,
                _ => unreachable!(),
            }
        }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        };
    }
    let result = match &cli.command {
        CliCommand::Logs(command) => return commands::logs::run(command),
        CliCommand::Agent(AgentCommand::Call(args)) => commands::agent::call(args).map(drop),
        CliCommand::Package(args) => commands::package::run(args).map(drop),
        CliCommand::InstallAndroid(args) => commands::install_android::run(args),
        CliCommand::Steam(commands::steam::SteamCommand::Upload(args)) => {
            // Credentials live in the environment only (specs/24 § 3): never in Regia.toml,
            // never on disk, never in logs.
            commands::steam::SteamCredentials::from_env()
                .and_then(|credentials| commands::steam::upload(args, &credentials).map(drop))
        }
        CliCommand::Steam(commands::steam::SteamCommand::InputManifest(args)) => {
            commands::steam::input_manifest(args).map(|out| {
                if let Some(out) = out {
                    println!("steam input manifest: {}", out.display());
                }
            })
        }
        CliCommand::Steam(commands::steam::SteamCommand::Layout(args)) => commands::steam::layout(args),
        CliCommand::Cook(args) => commands::cook::run(args).map(drop),
        CliCommand::UpgradeProject(args) => commands::upgrade_project::run(args).map(drop),
        CliCommand::Preview(args) => commands::preview::run(args).map(drop),
        CliCommand::Run(args) => commands::run::run(args),
        CliCommand::Server(args) => commands::server::run(args),
        CliCommand::Render(args) => commands::render::run(args),
        CliCommand::BakeGi(args) => commands::bake_gi::run(args),
        CliCommand::Doctor(args) => commands::doctor::run(args).map(drop),
        CliCommand::Clean(args) => commands::clean::run(args),
        CliCommand::New(args) => commands::new_project::run(args).map(drop),
        CliCommand::BakeLightmaps(args) => commands::bake_lightmaps::run(args),
        CliCommand::ApkSize(args) => commands::apk_size::run(args),
        CliCommand::Table(command) => commands::table::run(command),
        CliCommand::Rules(command) => commands::rules::run(command),
        CliCommand::Terrain(_) => unreachable!(),
        CliCommand::DccFeed(command) => commands::dcc_feed::run(command),
        CliCommand::Procgen(command) => commands::procgen::run(command),
        CliCommand::Viewer(command) => commands::viewer::run(command),
        CliCommand::Assets(command) => commands::assets_query::run(command),
        CliCommand::Content(command) => commands::content::run(command),
        CliCommand::Loc(command) => commands::loc::run(command).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("localization check failed"))
            }
        }),
        CliCommand::Atlas(args) => commands::atlas::run(args),
        CliCommand::Media(command) => commands::media_encode::run(command),
        CliCommand::TiledAutotile(args) => commands::tiled_autotile::run(args).map(drop),
        CliCommand::Import(args) => commands::import::run(args),
        CliCommand::Fracture(args) => commands::fracture::run(args),
        CliCommand::Export(command) => commands::export::run(command),
        CliCommand::Plugin(command) => commands::plugin::run(command).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("extension check failed"))
            }
        }),
        CliCommand::Level(command) => commands::level::run(command),
        CliCommand::Trace(command) => commands::trace::run(command),
        CliCommand::Engine(command) => commands::engine::run(command),
        CliCommand::Editor(args) => commands::editor::run(args),
        CliCommand::Gen(command) => commands::r#gen::run(command),
        CliCommand::Describe(args) => commands::describe::run_describe(args),
        CliCommand::Preflight(args) => commands::describe::run_preflight(args).and_then(|passed| {
            if passed {
                Ok(())
            } else {
                Err(anyhow::anyhow!("regia preflight: the kind lint found errors"))
            }
        }),
        CliCommand::Golden(command) => commands::golden::run(command),
        CliCommand::BakeCaptures(args) => commands::bake_captures::run(args),
        CliCommand::Hooks(command) => commands::hooks::run(command),
        CliCommand::PlayGate(command) => commands::play_gate::run(command),
        CliCommand::Release(_) => unreachable!(),
        CliCommand::Prepare(_) => unreachable!(),
        CliCommand::Bench(_) => unreachable!(),
        CliCommand::Net(_) => unreachable!(),
        CliCommand::Mcp(_) => unreachable!(),
        CliCommand::Test(_) => unreachable!(),
        CliCommand::ContactSheet(_) => unreachable!(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            // Stable exit code of a usage error and of an unsupported server platform (spec 76 § 3).
            if err.is::<commands::assets_validate::UsageError>()
                || err.is::<commands::package::ServerPlatformUnsupported>()
                || err.is::<commands::play_gate::PlayGateUsage>()
            {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// Runs `regia package --json`, `regia run --json` and a plain `regia cook --json` in the one-object
/// mode ([`json_out`]); `None` for every other command. A failure prints `ok: false` with an `error`
/// code (`package_failed`, `run_failed`, `cook_failed`, or `server_platform_unsupported` with exit
/// code 2) after the usual `error:` line on stderr.
fn run_json_command(command: &CliCommand) -> Option<ExitCode> {
    let (outcome, code) = match command {
        CliCommand::Package(args) if args.json => {
            json_out::set(true);
            let outcome = if args.audit_dir.is_some() {
                commands::package::audit_existing(args).map(Some)
            } else if args.audit_artifact.is_some() {
                commands::package::audit_artifact_only(args).map(Some)
            } else {
                commands::package::run(args)
                    .map(|artifact| Some(commands::package::json_result(args, &artifact)))
            };
            (outcome, "package_failed")
        }
        CliCommand::Run(args) if args.json => {
            json_out::set(true);
            (commands::run::run_json(args).map(Some), "run_failed")
        }
        CliCommand::Cook(args) if args.plain_json() => {
            json_out::set(true);
            // The cook prints its own object on success.
            (commands::cook::run(args).map(|()| None), "cook_failed")
        }
        _ => return None,
    };
    Some(match outcome {
        Ok(value) => {
            let ok = value.as_ref().is_none_or(|v| v["ok"] == true);
            if let Some(value) = value {
                json_out::emit(&value);
            }
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            let server = err.is::<commands::package::ServerPlatformUnsupported>();
            let code = if server {
                "server_platform_unsupported"
            } else if let Some(container) = err.downcast_ref::<commands::package_container::ContainerError>()
            {
                // `regia package --container`'s stable codes (ADR-8835).
                container.code
            } else {
                code
            };
            if let Some(stop) = err.downcast_ref::<commands::assets_validate::LintStop>() {
                json_out::emit(&serde_json::json!({
                    "ok": false,
                    "error": {"code": stop.code, "message": stop.message},
                    "lint": {"status": stop.status, "report": stop.report},
                }));
            } else if let Some(hygiene) = err.downcast_ref::<regia_cli::package_hygiene::AuditError>() {
                json_out::emit(&serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": hygiene.code,
                        "message": hygiene.location,
                        "location_id": hygiene.location_id,
                        "matcher_bytes": hygiene.matcher_bytes,
                        "matchers_compiled": hygiene.matchers_compiled,
                    },
                    "hygiene": hygiene.report,
                }));
            } else {
                json_out::emit(&json_out::failure(code, &err));
            }
            if server {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommands_parse() {
        assert!(
            Cli::try_parse_from(["regia", "package", "--platform", "android", "--format", "apk"]).is_ok()
        );
        assert!(Cli::try_parse_from(["regia", "install-android", "--device", "X", "--no-logcat"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "agent", "call", "agent.hello"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "bench", "synthetic_cubes", "--size", "64x36", "--json"]).is_ok()
        );
        assert!(Cli::try_parse_from(["regia", "bench", "--size", "0x36"]).is_err());
        assert!(
            Cli::try_parse_from([
                "regia",
                "render",
                "--job",
                "cine/trailer.render_job.ron",
                "--date",
                "2026-10-28",
                "--dry-run",
                "--job-timeout-s",
                "60",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "agent", "agent.hello"]).is_err(),
            "the spec form is `agent call`"
        );
        assert!(
            Cli::try_parse_from([
                "regia",
                "agent",
                "call",
                "scene.query_entities",
                r#"{"name":"Player"}"#,
                "--project",
                "/p"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["regia", "mcp"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "mcp", "--project", "/p", "--attach"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "doctor"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "regia",
                "trace",
                "export",
                "a.regiatrace",
                "--format",
                "trace-event"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "bake-gi", "cornell", "--rays", "64", "--out", "c.fgiv"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "bake-gi"]).is_err(),
            "a scene is required"
        );
        assert!(Cli::try_parse_from(["regia", "bake-gi", "--from-scene", "a.level.ron", "--check"]).is_ok());
        match Cli::try_parse_from(["regia", "bake-gi", "cornell", "--check-volume", "some.fgiv"])
            .expect("check-volume parses")
            .command
        {
            CliCommand::BakeGi(args) => {
                assert_eq!(
                    args.check_volume.as_deref(),
                    Some(std::path::Path::new("some.fgiv"))
                );
            }
            other => panic!("not bake-gi: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["regia", "bake-gi", "cornell", "--check", "some.fgiv"]).is_err(),
            "--check takes no volume path"
        );
        assert!(
            Cli::try_parse_from(["regia", "preview", "a", "--out", "x.png"]).is_err(),
            "preview has no --out capture"
        );
        assert!(Cli::try_parse_from(["regia", "preview", "a", "--exit-after-frames", "1"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "regia",
                "bake-gi",
                "--from-scene",
                "a.level.ron",
                "--check",
                "--bake"
            ])
            .is_err(),
            "--check conflicts with --bake"
        );
        assert!(Cli::try_parse_from(["regia", "cook", "--stats"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "cook", "--project", "/p", "--stats"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "new", "My Game", "--template", "blank", "--path", "/tmp"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "new"]).is_err(),
            "a name is required"
        );
        assert!(Cli::try_parse_from(["regia", "doctor", "--platform", "android"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "package", "--platform", "linux", "--steam"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "package", "--server", "--platform", "windows"]).is_ok());
        let argv = |args: &[&str]| args.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(server_package_on_macos(&argv(&[
            "regia",
            "package",
            "--server",
            "--platform",
            "macos"
        ])));
        assert!(server_package_on_macos(&argv(&[
            "regia",
            "package",
            "--platform=macos",
            "--server"
        ])));
        assert!(!server_package_on_macos(&argv(&[
            "regia",
            "package",
            "--platform",
            "macos"
        ])));
        assert!(!server_package_on_macos(&argv(&[
            "regia",
            "package",
            "--server",
            "--platform",
            "linux"
        ])));
        assert!(
            Cli::try_parse_from([
                "regia",
                "server",
                "--project",
                "/p",
                "--level",
                "a.level.ron",
                "--port",
                "7000",
                "--clients",
                "2",
                "--agent"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["regia", "server", "--clients", "x"]).is_err());
        let profile = |args: &[&str]| match Cli::try_parse_from(args).expect("parses").command {
            CliCommand::Package(args) => args.profile_name,
            _ => panic!("not package"),
        };
        assert_eq!(profile(&["regia", "package", "--platform", "linux"]), "dev");
        assert_eq!(
            profile(&["regia", "package", "--server", "--platform", "linux"]),
            "server",
            "--server defaults to the built-in server profile"
        );
        assert_eq!(
            profile(&[
                "regia",
                "package",
                "--server",
                "--platform",
                "linux",
                "--profile",
                "dev"
            ]),
            "dev"
        );
        assert!(Cli::try_parse_from(["regia", "steam", "upload", "--branch", "beta"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "steam", "upload", "--branch", "default", "--live"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "steam", "input-manifest", "--stdout"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "regia",
                "steam",
                "layout",
                "--family",
                "switch",
                "--allow-keyboard-only",
                "Console",
                "--allow-keyboard-only",
                "Screenshot",
                "--check",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "steam", "deploy", "--branch", "beta"]).is_err(),
            "the spec form is `steam upload`"
        );
        assert!(Cli::try_parse_from(["regia", "frobnicate"]).is_err());
        assert!(Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--check"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--status"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "regia",
                "prepare",
                "--cache-dir",
                "/tmp/p",
                "--force",
                "--threads",
                "2",
                "--no-nice",
                "--profile",
                "DesktopHigh",
                "--timeout-secs",
                "30"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "regia",
                "prepare",
                "--cache-dir",
                "/tmp/p",
                "--profile",
                "desktop-high"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--profile", "nope"]).is_err(),
            "an unknown profile is a parse error"
        );
        assert!(
            Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--threads", "0"]).is_err(),
            "threads start at 1"
        );
        assert!(
            Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--check", "--force"]).is_err(),
            "--check does not warm"
        );
        assert!(
            Cli::try_parse_from(["regia", "prepare", "--cache-dir", "/tmp/p", "--status", "--check"])
                .is_err()
        );
    }

    #[test]
    fn editor_and_gen_subcommands_parse() {
        assert!(Cli::try_parse_from(["regia", "editor"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "editor", "game", "--headless", "--no-gpu"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "editor", "--project", "game", "--headless"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "editor", "--no-gpu"]).is_err(),
            "--no-gpu needs --headless"
        );
        assert!(
            Cli::try_parse_from(["regia", "editor", "a", "--project", "b"]).is_err(),
            "one project"
        );
        assert!(Cli::try_parse_from(["regia", "mcp", "--headless", "--no-gpu"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "mcp", "--headless", "--attach"]).is_err(),
            "--attach never starts an editor"
        );
        assert!(
            Cli::try_parse_from(["regia", "mcp", "--no-gpu"]).is_err(),
            "--no-gpu needs --headless"
        );
        assert!(
            Cli::try_parse_from(["regia", "mcp", "--editor-timeout", "5"]).is_err(),
            "--editor-timeout needs --headless"
        );
        assert!(Cli::try_parse_from(["regia", "golden", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "golden", "accept", "7"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "golden", "reject", "7", "--note", "no"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "golden", "accept", "7", "--yes"]).is_err(),
            "accepting a golden has no way to skip the question"
        );
        assert!(Cli::try_parse_from(["regia", "gen", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "gen", "run", "rocks", "ground"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "gen", "run", "--all"]).is_ok());
        assert!(
            Cli::try_parse_from(["regia", "gen", "run"]).is_err(),
            "a name or --all"
        );
        assert!(Cli::try_parse_from(["regia", "gen", "run", "rocks", "--all"]).is_err());
        assert!(Cli::try_parse_from(["regia", "gen", "verify"]).is_ok());
        assert!(Cli::try_parse_from(["regia", "gen", "verify", "rocks", "--json", "--project", "p"]).is_ok());
    }

    #[test]
    fn contact_sheet_help_lists_examples_and_exit_codes() {
        let mut command = Cli::command();
        let sub = command.find_subcommand_mut("contact-sheet").expect("subcommand");
        let mut long = Vec::new();
        sub.write_long_help(&mut long).expect("long help");
        let long = String::from_utf8(long).expect("utf-8");
        assert!(long.contains("Examples:"), "{long}");
        assert!(long.contains("Exit codes:"), "{long}");
        let mut short = Vec::new();
        sub.write_help(&mut short).expect("short help");
        let short = String::from_utf8(short).expect("utf-8");
        assert!(short.contains("Examples:"), "{short}");
        assert!(short.contains("Exit codes:"), "{short}");
    }
}
