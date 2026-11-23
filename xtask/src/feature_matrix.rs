// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask check-features`: compile the optional Cargo features the default build never builds
//! (ADR-2322, `docs/04-conventions.md` § 11).
//!
//! `cargo clippy --workspace --all-targets` builds each crate once, with the features the workspace
//! unifies (defaults plus whatever another member or a dev-dependency turns on). Code behind a feature
//! nothing turns on (`dlss`, the Steamworks backend, the media codecs, `lua`, the umbrella's re-exports)
//! and the off side of a feature a dev-dependency turns on (`regia_render` without `frame-gen` and
//! `test-hooks`, which every shipped game builds) were never compiled by the gate. [`MATRIX`] lists one
//! entry per such feature set; this command runs `cargo clippy -p <crate> --features <set> -- -D warnings`
//! (or `cargo check` with `--check`) for each, one after the other in the shared target directory, and
//! lists every failing set at the end.
//!
//! [`coverage_findings`] keeps the table honest: every `[features]` key of every `crates/*` manifest must
//! be named by an entry, be a default feature whose off side an entry with `no_default` builds, or be in
//! [`OUTSIDE`] with the reason it is not compiled here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

/// What a feature set needs from the host before it can be compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// Nothing beyond the workspace toolchain.
    Nothing,
    /// A program on `PATH` (`nasm` for the x86 AV1 assembly).
    Tool(&'static str),
    /// A host operating system (`std::env::consts::OS`): the set compiles only there.
    HostOs(&'static str),
}

/// One row of the matrix: a crate built with one feature set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureSet {
    /// Short unique name (`--set <name>`, the summary).
    pub name: &'static str,
    /// The package (`-p`).
    pub package: &'static str,
    /// `--features`, comma-joined; empty for none.
    pub features: &'static [&'static str],
    /// `--no-default-features`.
    pub no_default: bool,
    /// `--all-targets` (tests and benches gated on the feature); off where a dev-dependency would turn
    /// the very feature on whose off side the row exists for.
    pub all_targets: bool,
    /// What the host must have; a row whose need is unmet is skipped and says why.
    pub needs: Needs,
    /// Extra environment for the cargo run (a stub SDK path).
    pub env: &'static [(&'static str, &'static str)],
    /// The code this row compiles that nothing else does.
    pub why: &'static str,
}

const fn set(name: &'static str, package: &'static str, features: &'static [&'static str]) -> FeatureSet {
    FeatureSet {
        name,
        package,
        features,
        no_default: false,
        all_targets: false,
        needs: Needs::Nothing,
        env: &[],
        why: "",
    }
}

impl FeatureSet {
    const fn tests(mut self) -> Self {
        self.all_targets = true;
        self
    }

    const fn bare(mut self) -> Self {
        self.no_default = true;
        self
    }

    const fn needs(mut self, needs: Needs) -> Self {
        self.needs = needs;
        self
    }

    const fn env(mut self, env: &'static [(&'static str, &'static str)]) -> Self {
        self.env = env;
        self
    }

    const fn why(mut self, why: &'static str) -> Self {
        self.why = why;
        self
    }

    /// The cargo arguments (after `cargo`). `clippy` picks `cargo clippy ... -- -D warnings` over
    /// `cargo check`; `jobs` adds `-j`.
    pub fn cargo_args(&self, clippy: bool, jobs: Option<usize>) -> Vec<String> {
        let mut args: Vec<String> = vec![if clippy { "clippy" } else { "check" }.into()];
        args.extend(["--locked".into(), "-p".into(), self.package.into()]);
        if self.no_default {
            args.push("--no-default-features".into());
        }
        if !self.features.is_empty() {
            args.push("--features".into());
            args.push(self.features.join(","));
        }
        if self.all_targets {
            args.push("--all-targets".into());
        }
        if let Some(jobs) = jobs {
            args.push("-j".into());
            args.push(jobs.to_string());
        }
        args.extend(["--message-format".into(), "short".into()]);
        if clippy {
            args.extend(["--".into(), "-D".into(), "warnings".into()]);
        }
        args
    }

    /// The command line, for `--list` and the failure summary.
    pub fn display(&self, clippy: bool, jobs: Option<usize>) -> String {
        let env: String = self.env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        format!("{env}cargo {}", self.cargo_args(clippy, jobs).join(" "))
    }

    /// Why this host cannot build the row, or `None` when it can. `on_path` answers whether a program
    /// is on `PATH`.
    pub fn unmet(&self, os: &str, on_path: impl Fn(&str) -> bool) -> Option<String> {
        match self.needs {
            Needs::Nothing => None,
            Needs::Tool(tool) => (!on_path(tool)).then(|| format!("needs `{tool}` on PATH")),
            Needs::HostOs(want) => (os != want).then(|| format!("compiles on {want} only")),
        }
    }
}

/// The stub NGX binding for the DLSS rows (`crates/regia_dlss/check_stub/`): the build script uses it
/// instead of bindgen over a real SDK, so `ngx.rs` is type-checked and linted without NVIDIA's SDK. A
/// check-only path: a build that links fails on the missing NGX symbols.
pub const DLSS_STUB_ENV: (&str, &str) = ("REGIA_DLSS_CHECK_STUB", "1");

const DLSS_STUB: &[(&str, &str)] = &[DLSS_STUB_ENV];

/// A stand-in OIDN install for the denoise rows: the `oidn` build script accepts any `OIDN_DIR` and only
/// emits a link path, which `cargo check` never uses, so no Open Image Denoise library is needed. The
/// directory must exist (a missing `rerun-if-changed` path would rerun the script on every row).
/// `{root}` is the workspace root.
pub const OIDN_STUB_ENV: (&str, &str) = ("OIDN_DIR", "{root}/xtask/check_stubs/oidn");

const OIDN_STUB: &[(&str, &str)] = &[OIDN_STUB_ENV];

/// `value` with `{root}` replaced by the workspace root.
pub fn expand_env(value: &str, root: &Path) -> String {
    value.replace("{root}", &root.to_string_lossy())
}

/// The `regia` umbrella's features the all-features row turns on: every optional feature except the
/// ones that need a host the row cannot assume (`static-dxc`: Windows; `media-codecs-asm`: `nasm`).
pub const REGIA_ALL: &[&str] = &[
    "server",
    "net",
    "media",
    "dcc-feed",
    "steam",
    "lua",
    "logic",
    "crash",
    "physics2d",
    "frame-gen",
    "dlss",
    "voice-net",
    "xr",
    "media-codecs",
    "content-http",
    "content-signing",
    "voice-opus",
    "accesskit",
    "dev-assets",
    "explosion-fx",
    "fluid",
    "media-encode",
    "net-quic",
    "destruction-fx",
    "xess",
];

/// The matrix, in run order: the leaf crates first (cheap, and a break there is named at its source),
/// then the `regia` umbrella one feature at a time, then the umbrella with everything compatible on.
pub const MATRIX: &[FeatureSet] = &[
    // The light product viewer's looks and stills (ADR-2538, ADR-2587).
    set("looks-gpu", "regia_looks", &["gpu"])
        .tests()
        .why("the standalone look renderer and the pixel tests that measure it"),
    set("looks-viewer", "regia_looks", &["viewer"])
        .tests()
        .why("the looks as regia_viewer hooks, without the standalone renderer"),
    set("viewer-tools", "regia_viewer", &["tools"])
        .why("PNG stills for the agent surface (Still::encode_png)"),
    set("viewer-pack-cook", "regia_viewer_pack", &["cook"])
        .why("the desktop cook: `regia viewer pack` and `regia viewer standin`; a player builds without it"),
    set("configurator-fs", "regia_configurator", &["fs"])
        .why("reading content from a directory, on desktop and Android hosts; the browser builds without it"),
    // regia_render: the DLSS hook (stub and linked binding) and the shipped game's build.
    set("render-shipping", "regia_render", &[])
        .why("regia_render as games build it: without frame-gen and test-hooks, which its own tests turn on"),
    set("render-dev-assets", "regia_render", &["dev-assets"]).why("engine shader watching (shader_reload)"),
    set("render-static-dxc", "regia_render", &["static-dxc"])
        .needs(Needs::HostOs("windows"))
        .why("the statically linked DXC (Windows only; builds mach-dxcompiler)"),
    set("render-dlss", "regia_render", &["dlss"])
        .why("the DLSS device hook and DlssNgx over the stub binding"),
    set("render-dlss-linked", "regia_render", &["dlss"])
        .env(DLSS_STUB)
        .why("the linked DLSS path (cfg regia_dlss_linked) over the check stub of the NGX binding"),
    set("dlss-linked", "regia_dlss", &["sdk"])
        .tests()
        .env(DLSS_STUB)
        .why("ngx.rs and the gpu_smoke test over the check stub of the NGX binding"),
    set("dlss-stub", "regia_dlss", &["sdk"])
        .tests()
        .why("the sdk feature without DLSS_SDK: the stub, and the gpu_smoke test"),
    set("steamworks", "regia_steam", &["steamworks"])
        .tests()
        .why("the real Steamworks backend and the real_client test"),
    set("media-codecs", "regia_media", &["video", "voice-opus"])
        .tests()
        .why("the AV1, Opus and Vorbis decoders, voice Opus, their tests and the fuzz corpus"),
    set("media-av1", "regia_media", &["av1"])
        .tests()
        .why("AV1 alone (the codec tests' cfg(any) arms)"),
    set("media-opus", "regia_media", &["opus"])
        .tests()
        .why("Opus alone"),
    set("media-vorbis", "regia_media", &["vorbis"])
        .tests()
        .why("Vorbis alone"),
    set("media-av1-nasm", "regia_media", &["av1-nasm"])
        .needs(Needs::Tool("nasm"))
        .why("the x86 AV1 assembly"),
    set("media-encode", "regia_media", &["media-encode", "video"])
        .tests()
        .why("the AV1/Opus WebM encoder and its read-back tests through the decoders (ADR-3369)"),
    set("agent-bare", "regia_agent", &[])
        .bare()
        .why("the agent without the XR surface"),
    set("agent-dcc-feed", "regia_agent", &["dcc-feed"])
        .why("dcc-feed without dcc-feed-assets (a runtime build)"),
    set("agent-dcc-feed-assets", "regia_agent", &["dcc-feed-assets"])
        .why("the DCC feed with mesh, texture and clip items"),
    set(
        "agent-reference-trace",
        "regia_agent",
        &["reference-trace", "test-hooks"],
    )
    .why("seeing.reference_trace and render.fault_inject"),
    set("agent-media-encode", "regia_agent", &["media-encode"])
        .tests()
        .why("render-out format webm and its lavapipe test (ADR-3369)"),
    set("agent-denoise", "regia_agent", &["denoise"])
        .env(OIDN_STUB)
        .why("OIDN previews through the agent (stub OIDN_DIR: nothing links)"),
    set("pt-denoise", "regia_pt", &["denoise"])
        .tests()
        .env(OIDN_STUB)
        .why("regia_pt::denoise and its test (stub OIDN_DIR: nothing links)"),
    set("pt-lightmap-gpu", "regia_pt", &["lightmap-gpu"])
        .tests()
        .why("the GPU lightmap baker and its tests"),
    set("cli-lightmap-gpu", "regia_cli", &["lightmap-gpu"]).why("regia bake-lightmaps --gpu"),
    set("cli-no-encode", "regia_cli", &[])
        .bare()
        .why("regia without its default media-encode: `regia media encode` answers media_encode_unavailable"),
    set("editor-hot-dylib", "regia_editor", &["hot-dylib"]).why("the editor linked through regia_dylib"),
    set("core-tracy", "regia_core", &["tracy"]).why("the Tracy profiler layer"),
    set(
        "physics-extras",
        "regia_physics",
        &["physics2d", "physics-cross-platform-determinism"],
    )
    .tests()
    .why("rapier2d bodies and enhanced determinism, and the 2D tests"),
    set("platform-bare", "regia_platform", &[])
        .bare()
        .why("the platform layer without a window"),
    set("platform-accesskit", "regia_platform", &["accesskit"]).why("the AccessKit adapters"),
    set("ui-accesskit", "regia_ui", &["accesskit"])
        .tests()
        .why("the screen-reader bridge and its test"),
    set("crash-tls", "regia_crash", &["tls"])
        .tests()
        .why("https:// crash uploads"),
    set("asset-extras", "regia_asset", &["content-http", "dev-assets"])
        .tests()
        .why("https:// content sources and file watching, and their tests"),
    set("asset-content-signing", "regia_asset", &["content-signing"])
        .tests()
        .why("Ed25519 checks of fetched content manifests without https:// sources (ADR-2710), and their tests"),
    set("asset-shipping", "regia_asset", &[]).why("regia_asset without file watching (a shipped game)"),
    set("net-voice", "regia_net", &["voice"])
        .tests()
        .why("the voice relay and its test"),
    set("net-plain", "regia_net", &[]).why("regia_net without voice (a server that carries no voice)"),
    set("net-core", "regia_net", &[])
        .bare()
        .tests()
        .why("regia_net without ecs: the ECS-free core a headless server links (spec 120 § 3.2, ADR-8032)"),
    set("ai-bare", "regia_ai", &[])
        .bare()
        .why("regia_ai without debug drawing (the server build)"),
    set("ai-probe", "regia_ai", &["spatial-query-probe"]).why("the SpatialQueryProbe previews"),
    set("procgen-pipeline", "regia_procgen", &["pipeline"])
        .tests()
        .why("graph saves and bakes, and their tests"),
    set("framework-bare", "regia_framework", &[])
        .bare()
        .why("the framework without client modules"),
    set("framework-net", "regia_framework", &["net", "functional-tests"])
        .tests()
        .why("networked characters and functional tests, and the net tests"),
    set("framework-explosion-fx", "regia_framework", &["explosion-fx"])
        .tests()
        .why("the crash layers and ground-shock shake of explosions, and their tests"),
    set("input-bare", "regia_input", &[])
        .bare()
        .why("input without gilrs devices"),
    set("ecs-bare", "regia_ecs", &[])
        .bare()
        .why("the ECS without debug names (a shipping build)"),
    set("app-bare", "regia_app", &[])
        .bare()
        .why("the app without debug names"),
    // The umbrella: one feature at a time over the defaults (its own cfg code and re-exports).
    set("regia-shipping", "regia", &["client"])
        .bare()
        .why("a shipping game: client only, no agent, no ui"),
    set("regia-lua", "regia", &["lua"]).why("regia::lua"),
    set("regia-explosion-fx", "regia", &["explosion-fx"]).why("explosion effects through the umbrella"),
    set("regia-xr", "regia", &["xr"]).why("regia::xr"),
    set("regia-media", "regia", &["media"]).why("regia::media without codecs"),
    set("regia-media-codecs", "regia", &["media-codecs", "voice-opus"]).why("regia::media with the codecs"),
    set("regia-media-encode", "regia", &["media-encode"])
        .why("the offline encoder and render-out webm through the umbrella"),
    set("regia-media-codecs-asm", "regia", &["media-codecs-asm"])
        .needs(Needs::Tool("nasm"))
        .why("the AV1 assembly through the umbrella"),
    set("regia-steam", "regia", &["steam"]).why("regia::steam"),
    set("regia-crash", "regia", &["crash"]).why("regia::crash"),
    set("regia-net", "regia", &["net"]).why("regia::net"),
    set("regia-voice-net", "regia", &["voice-net"]).why("regia::net::voice"),
    set("regia-physics2d", "regia", &["physics2d"]).why("2D physics through the umbrella"),
    set("regia-frame-gen", "regia", &["frame-gen"]).why("frame generation through the umbrella"),
    set("regia-dlss", "regia", &["dlss"]).why("DLSS through the umbrella"),
    set("regia-content-http", "regia", &["content-http"])
        .why("https:// content sources through the umbrella"),
    set("regia-content-signing", "regia", &["content-signing"])
        .why("signed content manifests through the umbrella, without https:// sources (ADR-2710)"),
    set("regia-accesskit", "regia", &["accesskit"]).why("the screen-reader bridge through the umbrella"),
    set("regia-dev-assets", "regia", &["dev-assets"]).why("dev-run asset reload"),
    set("regia-dcc-feed", "regia", &["dcc-feed"]).why("the DCC feed through the umbrella"),
    set("regia-fluid", "regia", &["fluid"]).why("regia::fluid with its draw feature (ADR-2441)"),
    set("fluid-gpu", "regia_fluid", &["gpu"])
        .tests()
        .why("the particle liquid's wgpu compute path and its tolerance test against the CPU reference"),
    set("fluid-draw", "regia_fluid", &["draw"])
        .tests()
        .why("the particle liquid's MeshRenderer publishing of the surface and the whitewater"),
    set("fluid-physics", "regia_fluid", &["physics"])
        .tests()
        .why("the particle liquid's rigid-body coupling: moving solids and pressure impulses (ADR-2577)"),
    set("regia-static-dxc", "regia", &["static-dxc"])
        .needs(Needs::HostOs("windows"))
        .why("the static DXC through the umbrella (Windows only)"),
    set("regia-all", "regia", REGIA_ALL).why("every compatible umbrella feature at once, server included"),
    set("framework-media", "regia_framework", &["media"])
        .tests()
        .why("boot movies from Regia.toml (regia_framework::boot_movies, ADR-2410) and their tests"),
    set("net-quic", "regia_net", &["net-quic"])
        .tests()
        .why("QUIC datagrams (RFC 9221) under the existing reliability layer (ADR-3970)"),
    set("regia-net-quic", "regia", &["net-quic"]).why("regia::net QUIC datagram socket (ADR-3970)"),

    set("configurator-core", "regia_configurator", &[])
        .bare()
        .why("the product viewer contracts without the schema validator and its regex (ADR-2535)"),
    set("configurator-validate", "regia_configurator", &["validate"])
        .tests()
        .why("the contracts with the validator, and the contract tests that need it (ADR-2535)"),
    set("framework-destruction-fx", "regia_framework", &["destruction-fx"])
        .tests()
        .why("destruction sound, VFX and decal table effects and their tests (ADR-2734)"),
    set("ai-net", "regia_ai", &["net"])
        .bare()
        .tests()
        .why("connectionless server-bot AI without client debug drawing (ADR-3346)"),
    set("render-xess-stub", "regia_render", &["xess"]).tests()
        .why("XeSS hook and vendor adapter without the external Windows SDK (ADR-4656)"),
    set("xess-stub", "regia_xess", &["sdk"]).tests()
        .why("XeSS SDK-free contract and opt-in hardware smoke; SDK builds require external headers"),
    set("regia-xess", "regia", &["xess"]).why("XeSS through the umbrella"),
    set("core-cross-platform-determinism", "regia_core", &["cross-platform-determinism"])
        .tests()
        .why("glam's scalar math (glam/scalar-math) under regia_core and its tests (ADR-5176)"),
    set("animation-cross-platform-determinism", "regia_animation", &["cross-platform-determinism"])
        .tests()
        .why("the crowd and pose code on glam's scalar math, and their tests (ADR-5176)"),
    set("asset-pipeline-runtime", "regia_asset_pipeline", &[])
        .bare()
        .tests()
        .why("the player-build cook without the https client and the FBX/USD/SVG/Vorbis importers (ADR-7972)"),

    set("regia-server", "regia", &["server"])
        .bare()
        .why("the dedicated server build: no client, so regia_framework compiles without regia_vfx (lane w6-post05b-followups-z; add a regia-server-logic row once the logic-graph lanes merge and regia gains that feature)"),
    set("platform-accesskit-adapters", "regia_platform", &["accesskit-adapters"])
        .why("the AccessKit adapters without the runner's hooks: the editor window's WindowAdapter (ADR-7795)"),
    set("editor-no-accesskit", "regia_editor", &[])
        .bare()
        .why("the editor without its default screen-reader bridge: a11y_bridge's no-op run-loop calls (ADR-7746, ADR-7795)"),
    set("framework-visual-log", "regia_framework", &["visual-log"])
        .bare()
        .why("development visual logging without client/editor dependencies (ADR-7840); framework-bare verifies the shipping-off side"),
    set("regia-server-logic", "regia", &["server", "logic"])
        .bare()
        .why("a dedicated server that runs logic graphs: `logic` re-exports regia_ai and adds regia::logic_bridge without client (spec 102 P3; the logic-graph lanes' known issue, w11-logic-graph-finish)"),
    set("core-devtools", "regia_core", &["devtools"])
        .tests()
        .why("explicit developer environment gate and enabled-side tests (ADR-8149); default builds verify the shipping-off side"),
    set("detsim-serde", "regia_detsim", &["serde"])
        .tests()
        .why("serde derives on the lockstep hover parameters and state (ADR-8079); nothing in the workspace turns it on"),
    set("net-soak-desync-inject", "regia_net_soak", &["desync-inject"])
        .tests()
        .why("the lockstep fault hook (LockSim::perturb, the planted drift, the bot's --inject-desync-at) and the tests that plant a divergence: in a build exactly when the feature is named (ADR-8209)"),
    set("world-geom-stage-b", "regia_world_geom", &["stage_b"])
        .tests()
        .why("the additive native query API (Stage B), its column bench, edge-vector example and tests (ADR-8019, ADR-8246); nothing in the workspace turns it on"),
    set("world-geom-stats", "regia_world_geom", &["stats"])
        .tests()
        .why("caller-owned native query work counters over Stage B, and their tests (ADR-8246)"),
    set("nav-stage-b", "regia_nav", &["stage_b"])
        .tests()
        .why("the layered bake of a world-geometry map and its blocker and ground-edit feed, with its acceptance tests (ADR-8486)"),
    set("net-soak-core", "regia_net_soak", &[])
        .bare()
        .why("the soak kit on the ECS-free regia_net core, without the time harness: what the lockstep server's acceptance tests build on (ADR-8558)"),
];

/// Features deliberately not in [`MATRIX`], as `(package, feature, reason)`; `*` is every feature of
/// the package.
pub const OUTSIDE: &[(&str, &str, &str)] = &[
    (
        "regia_dylib",
        "*",
        "forwards each `regia` feature one to one (tests/feature_forwarding.rs); the `regia` rows compile them",
    ),
    (
        "regia_render",
        "frame-gen",
        "on in every workspace build (regia_render's own dev-dependency); render-shipping builds the off side",
    ),
    (
        "regia_render",
        "test-hooks",
        "on in every workspace build (regia_render's own dev-dependency); render-shipping builds the off side",
    ),
    (
        "regia_world_geom",
        "fixtures",
        "test data: the crate's own tests turn it on (self dev-dependency); every non-test build compiles the off side",
    ),
    (
        "regia_world_geom",
        "allocation-tests",
        "the audited allocator for CPU-only tests: the crate's own tests turn it on (self dev-dependency, ADR-8246); every non-test build compiles the off side",
    ),
    (
        "regia_detsim",
        "fp-test-hooks",
        "a dev-only test hook: regia_framework's dev-dependency turns it on in every workspace test build (ADR-8526); every non-test build compiles the off side",
    ),
];

/// One `[features]` table: feature name to its items, `default` included.
pub type Features = BTreeMap<String, Vec<String>>;

/// The `[features]` table of every `crates/*/Cargo.toml` that has one, by package name.
pub fn workspace_features(root: &Path) -> Result<BTreeMap<String, Features>> {
    let crates = root.join("crates");
    let mut out = BTreeMap::new();
    let mut dirs: Vec<_> = std::fs::read_dir(&crates)
        .with_context(|| format!("reading {}", crates.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    dirs.sort();
    for dir in dirs {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text =
            std::fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
        let (name, features) =
            manifest_features(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        if !features.is_empty() {
            out.insert(name, features);
        }
    }
    Ok(out)
}

/// The package name and `[features]` table of one manifest.
pub fn manifest_features(manifest: &str) -> Result<(String, Features)> {
    let value: toml::Value = toml::from_str(manifest)?;
    let name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .context("no [package] name")?
        .to_string();
    let features = value
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .map(|(k, v)| {
                    let items = v
                        .as_array()
                        .map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    (k.clone(), items)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((name, features))
}

/// What is wrong between the declared features and the table: a feature no row names (and that is no
/// default with a `no_default` row, and not in `outside`), a row naming a package or feature that does
/// not exist, and duplicate row names. Empty when the table covers the workspace.
pub fn coverage_findings(
    declared: &BTreeMap<String, Features>,
    matrix: &[FeatureSet],
    outside: &[(&str, &str, &str)],
) -> Vec<String> {
    let mut findings = Vec::new();
    for (package, feature, reason) in outside {
        let known = declared
            .get(*package)
            .is_some_and(|f| *feature == "*" || f.contains_key(*feature));
        if !known {
            findings.push(format!(
                "OUTSIDE names `{package}/{feature}`, which is not declared"
            ));
        }
        if reason.trim().is_empty() {
            findings.push(format!("OUTSIDE `{package}/{feature}` gives no reason"));
        }
    }
    let mut names = BTreeSet::new();
    for row in matrix {
        if !names.insert(row.name) {
            findings.push(format!("matrix row `{}` appears twice", row.name));
        }
        let Some(features) = declared.get(row.package) else {
            findings.push(format!(
                "matrix row `{}`: package `{}` declares no features under crates/",
                row.name, row.package
            ));
            continue;
        };
        for feature in row.features {
            if !features.contains_key(*feature) {
                findings.push(format!(
                    "matrix row `{}`: `{}` has no feature `{feature}`",
                    row.name, row.package
                ));
            }
        }
        if row.no_default && !features.contains_key("default") {
            findings.push(format!(
                "matrix row `{}`: `{}` has no default features to turn off",
                row.name, row.package
            ));
        }
    }
    for (package, features) in declared {
        let rows: Vec<&FeatureSet> = matrix.iter().filter(|r| r.package == package).collect();
        let defaults: &[String] = features.get("default").map(Vec::as_slice).unwrap_or(&[]);
        for feature in features.keys().filter(|f| *f != "default") {
            let named = rows.iter().any(|r| r.features.contains(&feature.as_str()));
            let off_side = defaults.contains(feature) && rows.iter().any(|r| r.no_default);
            let excused = outside
                .iter()
                .any(|(p, f, _)| *p == package && (*f == "*" || f == feature));
            if !(named || off_side || excused) {
                findings.push(format!(
                    "`{package}/{feature}` is in no feature-matrix row (xtask/src/feature_matrix.rs MATRIX) \
                     and not in OUTSIDE: add a row so the gate compiles it"
                ));
            }
        }
    }
    findings
}

/// Arguments of `cargo xtask check-features`.
#[derive(ClapArgs, Debug, Clone, Default)]
pub struct Args {
    /// Print the rows (name, command, what each compiles) and exit.
    #[arg(long)]
    pub list: bool,
    /// Run only these rows (repeatable).
    #[arg(long = "set", value_name = "NAME")]
    pub sets: Vec<String>,
    /// Run only the rows of these packages (comma-separated; preflight `--changed` passes its scope).
    #[arg(long, value_delimiter = ',', value_name = "CRATES")]
    pub packages: Vec<String>,
    /// `cargo check` instead of `cargo clippy -- -D warnings` (faster; warnings do not fail).
    #[arg(long)]
    pub check: bool,
    /// Parallel rustc jobs per cargo run (`-j`); the rows themselves run one after the other.
    #[arg(long, short = 'j')]
    pub jobs: Option<usize>,
    /// Stop at the first failing row instead of running them all.
    #[arg(long)]
    pub fail_fast: bool,
    /// Only check that the table covers every declared feature; compile nothing.
    #[arg(long)]
    pub coverage_only: bool,
}

/// The rows `args` selects, in table order. Unknown `--set` names are returned as the error list.
pub fn select<'a>(matrix: &'a [FeatureSet], args: &Args) -> Result<Vec<&'a FeatureSet>, Vec<String>> {
    let unknown: Vec<String> = args
        .sets
        .iter()
        .filter(|s| !matrix.iter().any(|r| r.name == s.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(unknown);
    }
    Ok(matrix
        .iter()
        .filter(|r| args.sets.is_empty() || args.sets.iter().any(|s| s == r.name))
        .filter(|r| args.packages.is_empty() || args.packages.iter().any(|p| p == r.package))
        .collect())
}

/// How one row ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOutcome {
    /// Compiled (and linted) clean.
    Passed,
    /// Failed; the error lines of its output.
    Failed(Vec<String>),
    /// Not run on this host, and why.
    Skipped(String),
    /// Not run: an earlier row failed under `--fail-fast`.
    NotRun,
}

/// The lines of cargo's `--message-format short` output that name an error, at most `max`.
pub fn error_lines(output: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = output
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("error") || t.contains(": error") || t.contains("error[")
        })
        .filter(|l| !l.contains("could not compile") && !l.trim_start().starts_with("error: aborting"))
        .take(max)
        .map(str::to_string)
        .collect();
    if out.is_empty() {
        // No recognisable error line (a build script panic, a missing tool): the output's tail.
        let all: Vec<&str> = output.lines().collect();
        out = all[all.len().saturating_sub(max)..]
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    out
}

/// Runs `rows` through `runner` (success and output of one cargo run), skipping rows whose need is
/// unmet (`unmet` gives the reason). Without `fail_fast` every row runs.
pub fn execute(
    rows: &[&FeatureSet],
    fail_fast: bool,
    unmet: impl Fn(&FeatureSet) -> Option<String>,
    mut runner: impl FnMut(&FeatureSet) -> (bool, String),
) -> Vec<(RowOutcome, f64)> {
    let mut out = Vec::with_capacity(rows.len());
    let mut failed = false;
    for (i, row) in rows.iter().enumerate() {
        if failed && fail_fast {
            out.push((RowOutcome::NotRun, 0.0));
            continue;
        }
        if let Some(why) = unmet(row) {
            println!("[{}/{}] {} skipped ({why})", i + 1, rows.len(), row.name);
            out.push((RowOutcome::Skipped(why), 0.0));
            continue;
        }
        println!("[{}/{}] {} ...", i + 1, rows.len(), row.name);
        let t = Instant::now();
        let (ok, output) = runner(row);
        let secs = t.elapsed().as_secs_f64();
        if ok {
            println!("      ok ({secs:.1}s)");
            out.push((RowOutcome::Passed, secs));
        } else {
            let lines = error_lines(&output, 12);
            for line in &lines {
                eprintln!("      {line}");
            }
            eprintln!("      FAILED ({secs:.1}s)");
            failed = true;
            out.push((RowOutcome::Failed(lines), secs));
        }
    }
    out
}

/// The end-of-run table and the failure list.
pub fn summary(
    rows: &[&FeatureSet],
    outcomes: &[(RowOutcome, f64)],
    clippy: bool,
    jobs: Option<usize>,
) -> String {
    let mut text = String::from("check-features: rows\n");
    let mut failed = Vec::new();
    let mut total = 0.0;
    for (row, (outcome, secs)) in rows.iter().zip(outcomes) {
        total += secs;
        let label = match outcome {
            RowOutcome::Passed => format!("ok {secs:>7.1}s"),
            RowOutcome::Failed(_) => {
                failed.push((row, outcome));
                format!("FAILED {secs:.1}s")
            }
            RowOutcome::Skipped(why) => format!("skipped ({why})"),
            RowOutcome::NotRun => "not run".into(),
        };
        text.push_str(&format!("  {:<24} {label}\n", row.name));
    }
    text.push_str(&format!("  total {total:.1}s\n"));
    if !failed.is_empty() {
        text.push_str(&format!("check-features: {} row(s) FAILED\n", failed.len()));
        for (row, outcome) in failed {
            text.push_str(&format!("  {}: {}\n", row.name, row.display(clippy, jobs)));
            if let RowOutcome::Failed(lines) = outcome {
                for line in lines {
                    text.push_str(&format!("      {line}\n"));
                }
            }
        }
    }
    text
}

/// Whether `program` is an executable file in a `PATH` directory.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), program.to_string()]
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

/// Runs the check; the process exit code is the result.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let declared = workspace_features(root)?;
    let findings = coverage_findings(&declared, MATRIX, OUTSIDE);
    for finding in &findings {
        eprintln!("check-features: {finding}");
    }
    let clippy = !args.check;
    let rows = match select(MATRIX, args) {
        Ok(rows) => rows,
        Err(unknown) => {
            eprintln!(
                "check-features: no row named {} (see `cargo xtask check-features --list`)",
                unknown.join(", ")
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    if args.list {
        for row in &rows {
            let needs = row
                .unmet(std::env::consts::OS, on_path)
                .map(|w| format!(" [skipped here: {w}]"))
                .unwrap_or_default();
            println!("  {:<24} {}{needs}", row.name, row.display(clippy, args.jobs));
            println!("{}{}", " ".repeat(29), row.why);
        }
        return Ok(if findings.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if args.coverage_only {
        if findings.is_empty() {
            let count: usize = declared
                .values()
                .map(|f| f.keys().filter(|k| *k != "default").count())
                .sum();
            println!(
                "check-features: the matrix covers all {count} declared features ({} rows)",
                MATRIX.len()
            );
            return Ok(ExitCode::SUCCESS);
        }
        return Ok(ExitCode::FAILURE);
    }
    if rows.is_empty() {
        println!("check-features: no row selected (no matrix crate in the scope)");
        return Ok(if findings.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let outcomes = execute(
        &rows,
        args.fail_fast,
        |row| row.unmet(std::env::consts::OS, on_path),
        |row| {
            let out = Command::new(&cargo)
                .current_dir(root)
                .args(row.cargo_args(clippy, args.jobs))
                .envs(row.env.iter().map(|(k, v)| (*k, expand_env(v, root))))
                .stdin(Stdio::null())
                .output();
            match out {
                Ok(out) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    (out.status.success(), text)
                }
                Err(err) => (false, format!("error: running cargo: {err}")),
            }
        },
    );
    print!("{}", summary(&rows, &outcomes, clippy, args.jobs));
    let any_failed = outcomes.iter().any(|(o, _)| matches!(o, RowOutcome::Failed(_)));
    Ok(if any_failed || !findings.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(pairs: &[(&str, &[&str])]) -> Features {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.iter().map(|s| (*s).to_string()).collect()))
            .collect()
    }

    fn declared(pkgs: &[(&str, Features)]) -> BTreeMap<String, Features> {
        pkgs.iter().map(|(n, f)| ((*n).to_string(), f.clone())).collect()
    }

    #[test]
    fn the_real_workspace_is_covered_and_every_row_names_real_features() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let declared = workspace_features(root).expect("manifests");
        assert!(declared.contains_key("regia") && declared.contains_key("regia_render"));
        let findings = coverage_findings(&declared, MATRIX, OUTSIDE);
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn an_unnamed_feature_is_a_finding_until_a_row_names_it() {
        let d = declared(&[("a", features(&[("x", &[]), ("y", &[])]))]);
        let rows = [set("a-x", "a", &["x"])];
        let findings = coverage_findings(&d, &rows, &[]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("`a/y`"), "{findings:?}");
        let rows = [set("a-x", "a", &["x"]), set("a-y", "a", &["y"])];
        assert!(coverage_findings(&d, &rows, &[]).is_empty());
    }

    #[test]
    fn a_default_feature_is_covered_by_a_no_default_row_and_outside_excuses() {
        let d = declared(&[
            ("a", features(&[("default", &["w"]), ("w", &[])])),
            ("b", features(&[("p", &[]), ("q", &[])])),
        ]);
        // Without the bare row `a/w` (on by default) is never compiled off.
        let findings = coverage_findings(&d, &[], &[("b", "*", "forwarder")]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("`a/w`"));
        let rows = [set("a-bare", "a", &[]).bare()];
        assert!(coverage_findings(&d, &rows, &[("b", "p", "r"), ("b", "q", "r")]).is_empty());
    }

    #[test]
    fn stale_and_duplicate_rows_are_findings() {
        let d = declared(&[("a", features(&[("x", &[])]))]);
        let rows = [
            set("one", "a", &["x"]),
            set("one", "a", &["gone"]),
            set("two", "missing", &[]),
            set("three", "a", &[]).bare(),
        ];
        let findings = coverage_findings(&d, &rows, &[]);
        assert_eq!(findings.len(), 4, "{findings:#?}");
        assert!(findings.iter().any(|f| f.contains("`one` appears twice")));
        assert!(findings.iter().any(|f| f.contains("no feature `gone`")));
        assert!(findings.iter().any(|f| f.contains("package `missing`")));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("no default features to turn off"))
        );
        let outside = [("a", "x", ""), ("a", "gone", "r"), ("nope", "*", "r")];
        let findings = coverage_findings(&d, &[], &outside);
        assert_eq!(findings.len(), 3, "{findings:#?}");
        assert!(findings.iter().any(|f| f.contains("`a/x` gives no reason")));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("`a/gone`, which is not declared"))
        );
        assert!(
            findings
                .iter()
                .any(|f| f.contains("`nope/*`, which is not declared"))
        );
    }

    #[test]
    fn manifests_parse_name_and_multi_line_feature_arrays() {
        let text = r#"
[package]
name = "regia_x"
[features]
default = ["a"]
# a comment
a = [
    "dep:b",   # trailing
    "c/d",
]
e = []
[dependencies]
"#;
        let (name, f) = manifest_features(text).expect("parse");
        assert_eq!(name, "regia_x");
        assert_eq!(f.keys().collect::<Vec<_>>(), ["a", "default", "e"]);
        assert_eq!(f["a"], ["dep:b", "c/d"]);
    }

    #[test]
    fn the_command_line_carries_the_feature_set_and_the_lint_level() {
        let row = set("r", "regia_render", &["dlss", "dev-assets"]).tests();
        assert_eq!(
            row.display(true, Some(6)),
            "cargo clippy --locked -p regia_render --features dlss,dev-assets --all-targets -j 6 \
             --message-format short -- -D warnings"
        );
        let bare = set("b", "regia_ecs", &[]).bare().env(DLSS_STUB);
        assert_eq!(
            bare.display(false, None),
            "REGIA_DLSS_CHECK_STUB=1 cargo check --locked -p regia_ecs --no-default-features --message-format short"
        );
    }

    #[test]
    fn the_oidn_stub_directory_exists_and_expands_from_the_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let dir = expand_env(OIDN_STUB_ENV.1, root);
        assert!(!dir.contains("{root}"), "{dir}");
        assert!(Path::new(&dir).is_dir(), "{dir} must exist (rerun-if-changed)");
        assert_eq!(expand_env("{root}/x", Path::new("/w")), "/w/x");
    }

    #[test]
    fn unmet_needs_skip_with_a_reason() {
        let nasm = set("n", "regia_media", &["av1-nasm"]).needs(Needs::Tool("nasm"));
        assert_eq!(nasm.unmet("linux", |_| true), None);
        assert_eq!(
            nasm.unmet("linux", |_| false).as_deref(),
            Some("needs `nasm` on PATH")
        );
        let dxc = set("d", "regia_render", &["static-dxc"]).needs(Needs::HostOs("windows"));
        assert_eq!(dxc.unmet("windows", |_| false), None);
        assert_eq!(
            dxc.unmet("linux", |_| true).as_deref(),
            Some("compiles on windows only")
        );
        assert!(!on_path("regia-no-such-program-xyz"));
    }

    #[test]
    fn every_row_runs_and_each_failure_is_reported() {
        let rows = [
            set("one", "a", &[]),
            set("two", "b", &[]),
            set("three", "c", &[]),
            set("four", "d", &[]),
        ];
        let refs: Vec<&FeatureSet> = rows.iter().collect();
        let mut ran = Vec::new();
        let outcomes = execute(
            &refs,
            false,
            |r| (r.name == "three").then(|| "needs `x` on PATH".to_string()),
            |r| {
                ran.push(r.name);
                let ok = r.name != "two" && r.name != "four";
                (
                    ok,
                    format!(
                        "src/lib.rs:1:1: error[E0425]: cannot find value in {}\nerror: could not compile",
                        r.name
                    ),
                )
            },
        );
        assert_eq!(ran, ["one", "two", "four"]);
        assert_eq!(outcomes[0].0, RowOutcome::Passed);
        assert_eq!(outcomes[2].0, RowOutcome::Skipped("needs `x` on PATH".into()));
        assert_eq!(
            outcomes[1].0,
            RowOutcome::Failed(vec![
                "src/lib.rs:1:1: error[E0425]: cannot find value in two".into()
            ])
        );
        let table = summary(&refs, &outcomes, true, None);
        assert!(table.contains("check-features: 2 row(s) FAILED"), "{table}");
        assert!(table.contains("  two: cargo clippy --locked -p b"), "{table}");
        assert!(table.contains("  four: cargo clippy --locked -p d"), "{table}");
        assert!(table.contains("skipped (needs `x` on PATH)"), "{table}");
    }

    #[test]
    fn fail_fast_stops_after_the_first_failure() {
        let rows = [set("one", "a", &[]), set("two", "b", &[]), set("three", "c", &[])];
        let refs: Vec<&FeatureSet> = rows.iter().collect();
        let outcomes = execute(&refs, true, |_| None, |r| (r.name != "one", "boom".into()));
        assert!(matches!(outcomes[0].0, RowOutcome::Failed(_)));
        assert_eq!(outcomes[1].0, RowOutcome::NotRun);
        assert_eq!(outcomes[2].0, RowOutcome::NotRun);
    }

    #[test]
    fn error_lines_pick_errors_or_fall_back_to_the_tail() {
        let out = "   Checking a v0.1.0\ncrates/a/src/lib.rs:3:5: error: unused variable `x`\n\
                   crates/a/src/lib.rs:9:1: warning: dead code\nerror: could not compile `a` (lib) due to 1 previous error\n";
        assert_eq!(
            error_lines(out, 5),
            ["crates/a/src/lib.rs:3:5: error: unused variable `x`"]
        );
        let panic = "line1\nline2\nthread 'main' panicked at build.rs:4\n";
        assert_eq!(
            error_lines(panic, 2),
            ["line2", "thread 'main' panicked at build.rs:4"]
        );
    }

    #[test]
    fn selection_filters_by_name_and_package_and_rejects_unknown_names() {
        let args = Args {
            packages: vec!["regia_steam".into()],
            ..Args::default()
        };
        let rows = select(MATRIX, &args).expect("known");
        assert_eq!(rows.iter().map(|r| r.name).collect::<Vec<_>>(), ["steamworks"]);
        let args = Args {
            sets: vec!["regia-all".into()],
            ..Args::default()
        };
        assert_eq!(select(MATRIX, &args).expect("known").len(), 1);
        let args = Args {
            sets: vec!["no-such-row".into()],
            ..Args::default()
        };
        assert_eq!(select(MATRIX, &args).err(), Some(vec!["no-such-row".to_string()]));
    }

    #[test]
    fn the_all_features_row_names_every_umbrella_feature_but_the_host_bound_ones() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let declared = workspace_features(root).expect("manifests");
        let regia = &declared["regia"];
        let defaults = &regia["default"];
        let host_bound: BTreeSet<&str> = MATRIX
            .iter()
            .filter(|r| r.package == "regia" && r.needs != Needs::Nothing)
            .flat_map(|r| r.features.iter().copied())
            .collect();
        for feature in regia.keys().filter(|f| *f != "default") {
            let optional = !defaults.contains(feature);
            let in_all = REGIA_ALL.contains(&feature.as_str());
            if optional && !host_bound.contains(feature.as_str()) {
                // `client` is pulled in by the defaults; everything else must be named.
                assert!(in_all || feature == "client", "regia-all misses `{feature}`");
            }
        }
    }
}
