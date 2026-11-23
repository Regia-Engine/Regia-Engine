// SPDX-License-Identifier: MPL-2.0
//! Umbrella crate: re-exports, prelude, `DefaultPlugins` and `regia::run()`.
//!
//! A desktop game calls [`run`]; its Android crate uses [`android_main!`], which calls
//! `run_android`. Both set up logging, add [`DefaultPlugins`] and the game's plugin, and hand control to
//! the platform runner.
//!
//! Features: `client` (default) is everything a player's machine needs. `server` is the dedicated-server
//! build (spec 76): with `--no-default-features --features server` no window, render or audio crate is in
//! the dependency graph at all, and [`run`] hosts the game headless (`server`, `book/agents/net-dedicated-server.md`).
//!
//! # Getting started
//!
//! A game is a `Plugin`. [`run`] sets up logging, adds [`DefaultPlugins`] and the
//! game, and hands control to the platform runner until the window closes. This needs a display and
//! a GPU, so the example is compiled but not run:
//!
//! ```no_run
//! use regia::prelude::*;
//!
//! struct MyGame;
//!
//! impl Plugin for MyGame {
//!     fn build(&self, app: &mut App) {
//!         app.add_systems(Startup, hello);
//!     }
//! }
//!
//! fn hello() {
//!     info!("hello from my game");
//! }
//!
//! fn main() {
//!     regia::run(MyGame);
//! }
//! ```
//!
//! For a game in one file see `regia::quick` (feature `quick`).

#[cfg(not(any(feature = "client", feature = "server")))]
compile_error!(
    "regia needs the `client` feature (a game with `default-features = false` lists it) or the `server` feature"
);

/// The 2D toolkit (sprites, atlases, sprite animation, tilemaps, the sprite pass; specs/22 § T2.10).
/// Add `regia::two_d::Sprite2dPlugin` and `regia_framework::camera::ortho::Camera2dPlugin`.
#[cfg(feature = "client")]
pub use regia_2d as two_d;
#[cfg(feature = "agent")]
pub use regia_agent as agent;
#[cfg(any(feature = "server", feature = "logic"))]
pub use regia_ai as ai;
pub use regia_app as app;
#[cfg(feature = "client")]
pub use regia_audio as audio;
pub use regia_core as core;
#[cfg(feature = "crash")]
pub use regia_crash as crash;
pub use regia_ecs as ecs;
pub use regia_framework as framework;
/// Extensions (`docs/specs/57-plugin-extension-api.md`): the module, `extension!` and
/// `register_extensions!`.
#[cfg(feature = "client")]
pub use regia_framework::{extension, register_extensions};
#[cfg(feature = "server")]
pub use regia_nav as nav;
#[cfg(feature = "net")]
pub use regia_net as net;
pub use regia_physics as physics;
pub use regia_platform as platform;
/// Procedural content graphs and generators (`book/agents/procgen.md`): `ProcGenPlugin` is in
/// [`DefaultPlugins`] with the `procgen` feature (default on).
#[cfg(feature = "procgen")]
pub use regia_procgen as procgen;
#[cfg(feature = "client")]
pub use regia_render as render;
/// Structured engine telemetry (`book/user-guide/telemetry.md`): `TelemetryPlugin` is in
/// [`DefaultPlugins`]; `regia::telemetry::json_layer` is the one-line JSONL sink.
#[cfg(feature = "client")]
pub use regia_telemetry as telemetry;
pub use regia_world as world;
#[cfg(feature = "quick")]
pub mod quick;
/// Particle liquid: PIC/FLIP, whitewater, `.fluidcache` (the `fluid` feature, ADR-2441). Add
/// `regia::fluid::FluidPlugin` to the app yourself; nothing installs it.
#[cfg(feature = "fluid")]
pub use regia_fluid as fluid;
/// Video playback (specs/50, ADR-1670).
#[cfg(feature = "media")]
pub use regia_media as media;
/// Sandboxed Lua 5.1 mods (`docs/specs/84-lua-mods.md`, ADR-1751): `regia::lua::LuaModsPlugin`.
#[cfg(feature = "lua")]
pub use regia_script_lua as lua;
#[cfg(feature = "steam")]
pub use regia_steam as steam;
#[cfg(feature = "ui")]
pub use regia_ui as ui;
/// OpenXR head-mounted displays (`docs/specs/58-openxr.md`): `regia::xr::XrPlugin`.
#[cfg(feature = "xr")]
pub use regia_xr as xr;

/// Quits through the engine's clean exit (ADR-188): leave fullscreen, wait (bounded) for the GPU,
/// drop the surfaces, the renderers and the device, save the pipeline cache, then return from
/// [`run`]. Closing the window and writing `AppExit` take the same path; see
/// [`regia_platform::run_clean_exit`] for the steps, their budgets and the host watchdog
/// ([`regia_platform::CleanExitSettings::watchdog`]).
#[cfg(feature = "client")]
pub use regia_platform::request_clean_exit;

use regia_app::prelude::*;

#[cfg(feature = "client")]
pub mod audio_listener;
#[cfg(feature = "client")]
pub mod audio_occlusion;
#[cfg(all(feature = "net", feature = "client"))]
pub mod dev_client;
#[cfg(feature = "net")]
pub mod lagcomp_bridge;
#[cfg(feature = "logic")]
pub mod logic_bridge;
#[cfg(feature = "net")]
pub mod net_relevance_bridge;
// Montage commands over the network (spec 34 § 9, ADR-9370); opt-in plugin.
#[cfg(feature = "net")]
pub mod montage_net;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "net")]
pub mod stream_relevance;

/// Everything a game normally imports: `use regia::prelude::*;`.
pub mod prelude {
    pub use crate::{DefaultPlugins, Profiler, run};
    pub use regia_app::prelude::*;
    #[cfg(feature = "client")]
    pub use regia_platform::RunnerSettings;
    pub use regia_platform::{
        AppLifecycle, BackPressed, ButtonState, ClosePolicy, CursorMoved, FileDropped, ImeEvent,
        InputTimestamp, InputTiming, KeyboardInput, LifecycleEvent, LifecycleState, MouseButton,
        MouseButtonInput, MouseMotion, MouseWheel, PresentMode, PrimaryWindow, SystemThemeChanged,
        TouchInput, TouchPhase, Window, WindowCloseRequested, WindowFocused, WindowMode, WindowResized,
        WindowScaleFactorChanged, WindowSettings,
    };
    pub use regia_platform::{
        Clipboard, Haptics, KeepScreenOn, OsInfo, SafeArea, Vfs, VfsPath, open_url, system_locale,
    };
    #[cfg(feature = "client")]
    pub use regia_render::ClearColor;
    #[cfg(feature = "ui")]
    pub use regia_ui::prelude::*;
    pub use tracing::{debug, error, info, trace, warn};
}

/// The engine's standard plugins: platform (window, lifecycle, input messages), renderer (with
/// gizmos and the debug-overlay drawers), assets, audio (entity emitters with physics occlusion,
/// reverb zones), telemetry (structured `regia::*` events, `book/user-guide/telemetry.md`), with
/// the `ui` feature the runtime UI, and with the `agent` feature the agent protocol server.
///
/// Audio opens the default output device; without one (CI, servers) it falls back to the
/// offline backend by itself. `REGIA_AUDIO=off` forces the offline backend (`headless`),
/// including when an `AudioSettings` resource was inserted first: that resource's other
/// fields (sample rate, buses, voice limit) stay, and `headless` becomes true.
///
/// A build without the `client` feature has no window, renderer or audio: there this is
/// `server::ServerPlugins`.
#[derive(Default)]
pub struct DefaultPlugins;

/// The process-wide frame profiler (`docs/specs/10-core.md` § 1.3), fed by the logging layer; frames
/// are closed in `Last`.
#[derive(Resource, Clone)]
pub struct Profiler(pub regia_core::profiling::FrameProfiler);

#[cfg(feature = "client")]
fn end_profiler_frame(profiler: Option<Res<Profiler>>) {
    if let Some(p) = profiler {
        p.0.end_frame();
    }
}

/// `AudioSettings` after `REGIA_AUDIO`. `off` forces the offline backend and keeps every other
/// field of a resource the game already inserted. Any other value (or an unset variable) leaves
/// that resource unchanged. With no resource and the variable unset, nothing is inserted.
#[cfg(feature = "client")]
pub(crate) fn audio_settings_for_env(
    existing: Option<regia_audio::AudioSettings>,
    regia_audio: Option<&str>,
) -> Option<regia_audio::AudioSettings> {
    let off = regia_audio.is_some_and(|value| value.eq_ignore_ascii_case("off"));
    match (existing, off) {
        (Some(mut settings), true) => {
            settings.headless = true;
            Some(settings)
        }
        (None, true) => Some(regia_audio::AudioSettings {
            headless: true,
            ..Default::default()
        }),
        (existing, false) => existing,
    }
}

#[cfg(not(feature = "client"))]
impl Plugin for DefaultPlugins {
    fn build(&self, app: &mut App) {
        app.add_plugins(server::ServerPlugins);
    }
}

/// The bus layout a project ships at `config/audio.ron` under `root` (spec 16 § 1). `None` when
/// there is no file, it does not parse as a [`regia_audio::BusLayout`], or it fails validation —
/// the caller keeps its current tree and a warning names the reason. A file that parses but
/// fails validation is a project error: silently building a half-tree would be worse than the
/// default.
#[cfg(feature = "client")]
fn audio_bus_layout_from(root: &std::path::Path) -> Option<regia_audio::BusLayout> {
    let file = root.join("config/audio.ron");
    if !file.is_file() {
        return None;
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(target: "regia_audio", "config/audio.ron could not be read: {error}");
            return None;
        }
    };
    let layout = match regia_audio::BusLayout::from_ron(&text) {
        Ok(layout) => layout,
        Err(error) => {
            tracing::warn!(target: "regia_audio", "config/audio.ron is not a valid bus layout: {error}");
            return None;
        }
    };
    if let Err(error) = layout.validate() {
        tracing::warn!(target: "regia_audio", "config/audio.ron failed validation, keeping the current tree: {error}");
        return None;
    }
    tracing::debug!(target: "regia_audio", buses = layout.buses.len(), "config/audio.ron loaded");
    Some(layout)
}

#[cfg(feature = "client")]
impl Plugin for DefaultPlugins {
    fn build(&self, app: &mut App) {
        regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
        // The hitch event reads the frame that just ended from the profiler, so the profiler
        // closes the frame after telemetry ran.
        app.add_plugins(regia_app::DiagnosticsPlugin::default())
            .add_systems(
                regia_app::Last,
                end_profiler_frame.after(regia_telemetry::TelemetrySystems),
            );
        // Structured telemetry (session header, frame summaries, hitches, GPU errors,
        // problems, streaming events). `REGIA_TELEMETRY=off|normal|verbose|ship`.
        if !app.world().contains_resource::<Profiler>() {
            app.insert_resource(Profiler(profiler()));
        }
        app.insert_resource(regia_telemetry::SpanProfiler(profiler()));
        app.add_plugins(regia_telemetry::TelemetryPlugin::default());
        // GPU breadcrumbs (on in every profile) and the trace recorder: idle until an agent, the
        // host or `--record-trace` starts a recording (spec 54 § 3, ADR-9070).
        app.add_plugins((
            regia_render::breadcrumbs::BreadcrumbsPlugin::default(),
            regia_telemetry::trace::TracePlugin::default(),
        ));
        // Data a frame may read at any moment (terrain heights where things spawn) stays paged
        // in on a machine short of memory: a frame never sleeps reading it back (the
        // 2026-10-08 soaks).
        app.add_plugins(regia_app::ResidencyPlugin::default());
        app.add_plugins((
            regia_platform::PlatformPlugin,
            regia_render::RenderPlugin::default(),
            // Physics/audio/nav-mesh gizmo overlays; idle until a group is enabled.
            regia_framework::debug_gizmos::DebugGizmosPlugin,
            // `regia run --level L`; adds nothing without `REGIA_START_LEVEL`.
            regia_framework::start_level::StartLevelPlugin,
            // First-launch quality detection; acts only on a game's persisted `GameSettings`.
            regia_framework::auto_quality::AutoQualityPlugin,
            regia_framework::thermal::ThermalGovernorPlugin,
            // Installs the open level's `look.baked_gi` volume in the renderer; idle without one.
            regia_framework::baked_gi_level::BakedGiLevelPlugin,
            // Cooked meshlets reach the visibility buffer; idle unless the profile turns it on.
            regia_framework::meshlet_bridge::MeshletBridgePlugin,
        ));
        // `[runtime]` of `Regia.toml`: pause on focus loss and keep-screen-on (spec 11 §§ 2-3, ADR-7070).
        app.add_plugins(regia_framework::project_runtime::ProjectRuntimePlugin::default());
        // `[game]` of `Regia.toml`: the pause key and the Paused screen, off until the game sets
        // `pause = true` (ADR-10104).
        app.add_plugins(regia_framework::game_pause::GamePausePlugin::default());
        // Installs the open level's `look.lightmap`, warms its variants and attaches its receivers;
        // idle without one (ADR-2313).
        app.add_plugins(regia_framework::lightmap_level::LightmapLevelPlugin);
        // Publishes the open level's `look.captures` bake for its local reflection captures; idle
        // without one (ADR-2860).
        app.add_plugins(regia_framework::local_capture_level::LocalCaptureLevelPlugin);
        // World streaming (spec 78); idle until a streamed level opens.
        app.add_plugins(regia_world::WorldStreamingPlugin);
        // Reloads changed level sources and layer definitions; idle in a packaged build.
        app.add_plugins(regia_framework::level_assets::LevelAssetsPlugin);
        // The project's loading-screen prefab while a level loads (`[project] loading_screen`,
        // ADR-7263); idle without one.
        app.add_plugins(regia_framework::loading_screen::LoadingScreenPlugin);
        // Level switch with cancel: reads the next level on a worker, frees the replaced one on
        // another (ADR-8027); idle until a switch is requested.
        app.add_plugins(regia_framework::level_switch::LevelSwitchPlugin);
        // Exposure zones at the active camera drive the renderer's eye; idle without zones.
        app.add_plugins(regia_framework::exposure_zones::ExposureZonesPlugin);
        // Assets: dev runs read the cooked cache; a packaged build automatically
        // mounts `content/*.fpak` next to the executable (desktop) or copied out
        // of the APK assets (Android). Games never configure this themselves.
        let settings = asset_settings(app);
        // Packaged media (boot movies, VideoPlayer sources) extracts next to the paks on Android
        // (`android_content_dir`); root the media resolver there so those sources open from the
        // APK (media audit G finding 1, ADR-4826). A game that inserted its own `MediaFiles`
        // keeps its root, and `BootMoviesPlugin` only fills an empty one.
        #[cfg(all(target_os = "android", feature = "media"))]
        if let Some(dir) = settings.content_dir.clone()
            && !app.world().contains_resource::<regia_media::MediaFiles>()
        {
            app.insert_resource(regia_media::MediaFiles::with_root(dir));
        }
        // Extracted next to the paks (`android_content_dir`): a client joins with it.
        #[cfg(all(target_os = "android", feature = "net"))]
        if let Some(file) = settings.content_dir.as_ref().map(|d| d.join("net-manifest.ron"))
            && file.is_file()
        {
            app.insert_resource(regia_net::session::NetManifestFile(file));
        }
        app.add_plugins(regia_asset::AssetPlugin { settings });
        // Chunk states and delivery (spec 40); does nothing without a content manifest.
        app.add_plugins((
            regia_asset::content::ContentPlugin::default(),
            regia_framework::chunk_level_warnings::ChunkLevelWarningsPlugin,
        ));
        let env = regia_core::brand::env_var("REGIA_AUDIO").ok();
        let existing = app.world_mut().remove_resource::<regia_audio::AudioSettings>();
        let mut settings = audio_settings_for_env(existing, env.as_deref());
        // A project's `config/audio.ron` is the bus tree the runtime builds (spec 16 § 1) — the
        // runtime counterpart of the editor mixer reading the same file (ADR-5699). It overrides
        // the default tree and a host-inserted `AudioSettings::buses`; without a valid file the
        // current tree is kept.
        if let Some(layout) = audio_bus_layout_from(&regia_asset::detect_project_root()) {
            let settings = settings.get_or_insert_with(regia_audio::AudioSettings::default);
            settings.buses = layout;
        }
        if let Some(settings) = settings {
            app.insert_resource(settings);
        }
        app.add_plugins((
            regia_audio::AudioPlugin,
            audio_occlusion::PhysicsAudioOcclusionPlugin,
            audio_listener::CameraAudioListenerPlugin,
        ));
        // Footsteps from animation foot notifies (ADR-2442); idle without a `FootstepBank`.
        app.add_plugins(regia_framework::footsteps::FootstepPlugin);
        #[cfg(feature = "ui")]
        app.add_plugins(regia_ui::UiPlugin {
            load_project_assets: true,
        });
        // UI sounds (ADR-1842): each `UiSoundRequest` plays the audio cue it names.
        #[cfg(feature = "ui")]
        app.add_systems(Update, play_ui_sounds);
        // The performance overlay: hidden unless the player (settings menu), the host default
        // (`PerformanceOverlay::default_mode`) or an agent turns it on.
        #[cfg(feature = "ui")]
        app.add_plugins(regia_framework::perf_overlay::PerfOverlayPlugin);
        #[cfg(feature = "procgen")]
        app.add_plugins(regia_procgen::ProcGenPlugin)
            .add_systems(
                Update,
                start_level_bake_dir
                    .after(regia_framework::start_level::open_start_level)
                    .before(regia_procgen::world::ProcGenSet::Lifecycle),
            )
            .add_systems(
                Update,
                (level_change_bake_dir, level_bake_dirs).before(regia_procgen::world::ProcGenSet::Lifecycle),
            );
        // Captions and conversations (spec 56, ADR-1697); inert until a caption arrives or a
        // conversation plays.
        #[cfg(feature = "ui")]
        app.add_plugins((
            regia_ui::captions::CaptionPlugin,
            regia_framework::conversation::ConversationPlugin,
        ));
        // Game widgets (ADR-2958): scoreboards from the rules, lobby lists from `regia_net`; inert
        // until such a widget exists.
        #[cfg(feature = "ui")]
        app.add_plugins(regia_framework::game_ui::GameUiBridgePlugin);
        // Dialogue lines (spec 36 § 7, ADR-1882); inert until a line plays.
        #[cfg(feature = "ui")]
        app.add_plugins(regia_framework::dialogue::DialoguePlugin);
        // The `net_relevance` gizmo group (spec 72 § 10, ADR-2665 (5)): off until the agent's
        // net.relevance_gizmos (or debug.set_gizmos) turns it on.
        #[cfg(feature = "net")]
        app.add_plugins(net_relevance_bridge::NetRelevanceBridgePlugin);
        // Inert until a `.netdemo` is opened (spec 103, ADR-3021).
        #[cfg(feature = "net")]
        app.add_plugins(regia_framework::net_replay::NetReplayPlugin);
        #[cfg(feature = "agent")]
        app.add_plugins(regia_agent::AgentPlugin);
        // `ui.*` agent methods reach the UI through this hook (the agent crate cannot depend on
        // `regia_ui`, ADR-202; book/agents/ui.md).
        #[cfg(all(feature = "agent", feature = "ui"))]
        app.insert_resource(regia_agent::UiAgentHost::new(regia_ui::agent_bridge::host_call));
    }
}

/// Plays the audio cue of every UI sound request. A cue that is not registered, or a missing
/// audio device, is logged once per request at debug level and otherwise ignored: a UI sound
/// never fails a frame.
#[cfg(feature = "ui")]
fn play_ui_sounds(
    mut requests: MessageReader<regia_ui::ui_sounds::UiSoundRequest>,
    audio: Option<ResMut<regia_audio::Audio>>,
) {
    let Some(mut audio) = audio else {
        requests.clear();
        return;
    };
    for r in requests.read() {
        if let Err(e) = audio.play_cue(&r.sound) {
            tracing::debug!(cue = %r.sound, error = %e, "ui sound not played");
        }
    }
}

/// Asset server settings for this process: the automatic defaults, plus the
/// Android content directory extracted from the APK assets.
#[cfg(feature = "client")]
fn asset_settings(app: &App) -> regia_asset::AssetServerSettings {
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        regia_asset::AssetServerSettings::default()
    }
    #[cfg(target_os = "android")]
    {
        let mut settings = regia_asset::AssetServerSettings::default();
        if let Some(dir) = android_content_dir(app) {
            settings.content_dir = Some(dir);
        }
        settings
    }
}

/// Extract `content/*.fpak` and `net-manifest.ron` from the APK assets into internal storage and
/// return the directory holding them (the pak mount root). Paks are copied out
/// once per launch; `AAssetManager` cannot mmap into `PakReader`'s file API.
/// The cooked permutation list goes with them: the render plugin reads it from the same directory.
#[cfg(all(target_os = "android", feature = "client"))]
fn android_content_dir(app: &App) -> Option<std::path::PathBuf> {
    use regia_platform::{AndroidAppHandle, ApkAssetMount, VfsMount};

    let handle = app.world().get_resource::<AndroidAppHandle>()?;
    let dest = handle.0.internal_data_path()?.join("content");
    std::fs::create_dir_all(&dest).ok()?;
    let mount = ApkAssetMount::new(handle.0.asset_manager(), "content");
    let entries = mount.list_dir("").ok()?;
    let (mut copied, mut kept) = (0usize, 0usize);
    let list = regia_render::permutation_policy::PERMUTATION_MANIFEST_FILE;
    if !entries.iter().any(|entry| entry.name == list) {
        // An update that dropped the list must not leave the last one checking the new content.
        let _ = std::fs::remove_file(dest.join(list));
    }
    extract_content_records(&mount, &entries, &dest);
    for entry in entries {
        if entry.name == list {
            if let Err(err) = extract_permutation_list(&mount, list, &dest.join(list)) {
                tracing::warn!(target: "regia", "cannot extract {list}: {err}");
            }
            continue;
        }
        if entry.is_dir || !(entry.name.ends_with(".fpak") || entry.name == "net-manifest.ron") {
            continue;
        }
        match extract_pak(&mount, &entry.name, &dest.join(&entry.name)) {
            Ok(true) => copied += 1,
            Ok(false) => kept += 1,
            // One bad pak is skipped: the others still mount.
            Err(err) => tracing::warn!(target: "regia", "cannot extract {}: {err}", entry.name),
        }
    }
    tracing::info!(
        target: "regia",
        paks = copied,
        unchanged = kept,
        dir = %dest.display(),
        "mounted APK asset paks"
    );
    // Packaged media (boot movies, VideoPlayer sources) is never cooked into paks: it ships as
    // raw APK assets under the assets root, listed by the package-time media index (an APK
    // assets directory cannot be walked for subdirectories). It is extracted next to the paks
    // at the layout the sources name — `<content>/movies/intro.webm`, so a documented
    // `movies/intro.webm` source resolves with `MediaFiles` rooted at this directory (specs/50
    // § 4) — and media the index no longer lists is pruned (media audit G finding 1, ADR-4826;
    // the index, the layout and the pruning amend it, ADR-5441).
    #[cfg(feature = "media")]
    {
        let media_mount = ApkAssetMount::new(handle.0.asset_manager(), "");
        match extract_packaged_media_indexed(&media_mount, &dest) {
            Ok((media_copied, media_kept)) => tracing::info!(
                target: "regia",
                media = media_copied,
                unchanged = media_kept,
                "extracted packaged media"
            ),
            Err(err) => tracing::warn!(target: "regia", "packaged media not extracted: {err}"),
        }
    }
    Some(dest)
}

/// Copies the content records from the APK next to the extracted paks: `ContentPlugin` reads them
/// from that directory. They are small, so each is compared whole (a rebuilt manifest can keep its
/// length). A record the APK no longer has is removed; a failure leaves that record out and logs it.
#[cfg(target_os = "android")]
fn extract_content_records(
    mount: &regia_platform::ApkAssetMount,
    entries: &[regia_platform::VfsDirEntry],
    dest: &std::path::Path,
) {
    use regia_asset::content::{
        CHUNK_MAP_FILE, CHUNK_WHY_FILE, CONTENT_HOST_FILE, CONTENT_MANIFEST_FILE, CONTENT_TRUST_FILE,
    };
    use regia_platform::VfsMount;
    use std::io::Read;

    for name in [
        CONTENT_MANIFEST_FILE,
        CHUNK_MAP_FILE,
        CHUNK_WHY_FILE,
        CONTENT_HOST_FILE,
        CONTENT_TRUST_FILE,
    ] {
        let path = dest.join(name);
        if !entries.iter().any(|e| e.name == name) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let copied = (|| -> std::io::Result<()> {
            let mut bytes = Vec::new();
            mount.open(name)?.read_to_end(&mut bytes)?;
            if std::fs::read(&path).is_ok_and(|old| old == bytes) {
                return Ok(());
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(err) = copied {
            tracing::warn!(target: "regia", "cannot extract {name}: {err}");
        }
    }
}

/// Copies one APK pak to `dest` unless an identical copy is already there (same length, same
/// first and last 4 KiB: the header with the TOC location and the TOC's tail). Streams in chunks
/// through a temporary file and a rename: no whole-pak buffer, and a launch killed mid-copy never
/// leaves a torn pak. Returns whether it copied. Every launch used to read and rewrite every pak
/// here, before the first frame (instant-load).
#[cfg(all(target_os = "android", feature = "client"))]
fn extract_pak(
    mount: &regia_platform::ApkAssetMount,
    name: &str,
    dest: &std::path::Path,
) -> std::io::Result<bool> {
    use regia_platform::VfsMount;
    use std::io::{Seek, SeekFrom, Write};

    let mut src = mount.open(name)?;
    let len = src.seek(SeekFrom::End(0))?;
    if let Ok(meta) = std::fs::metadata(dest)
        && meta.len() == len
    {
        let mut existing = std::fs::File::open(dest)?;
        if same_pak_bytes(&mut existing, src.as_mut(), len, name.ends_with(".fpak"))? {
            return Ok(false);
        }
    }
    src.seek(SeekFrom::Start(0))?;
    let tmp = dest.with_extension("fpak.tmp");
    let copied = (|| -> std::io::Result<()> {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        std::io::copy(&mut src, &mut out)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        std::fs::rename(&tmp, dest)
    })();
    if let Err(err) = copied {
        // A failed copy must not leave a torn temporary behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(true)
}

/// Whether two same-length copies of an APK file hold the same bytes. A pak compares its first and
/// last 4 KiB (the header, whose content hash covers the payload, and the end of the TOC) and the
/// whole TOC range the header names; any other file (`net-manifest.ron`) compares whole.
#[cfg(any(all(target_os = "android", feature = "client"), test))]
fn same_pak_bytes<A, B>(a: &mut A, b: &mut B, len: u64, is_pak: bool) -> std::io::Result<bool>
where
    A: std::io::Read + std::io::Seek + ?Sized,
    B: std::io::Read + std::io::Seek + ?Sized,
{
    use std::io::SeekFrom;

    const EDGE: u64 = 4096;
    fn range_eq<A, B>(a: &mut A, b: &mut B, start: u64, end: u64) -> std::io::Result<bool>
    where
        A: std::io::Read + std::io::Seek + ?Sized,
        B: std::io::Read + std::io::Seek + ?Sized,
    {
        a.seek(SeekFrom::Start(start))?;
        b.seek(SeekFrom::Start(start))?;
        let (mut left, mut buf_a, mut buf_b) = (end.saturating_sub(start), [0u8; 8192], [0u8; 8192]);
        while left > 0 {
            let n = left.min(buf_a.len() as u64) as usize;
            a.read_exact(&mut buf_a[..n])?;
            b.read_exact(&mut buf_b[..n])?;
            if buf_a[..n] != buf_b[..n] {
                return Ok(false);
            }
            left -= n as u64;
        }
        Ok(true)
    }
    if !is_pak {
        return range_eq(a, b, 0, len);
    }
    if !range_eq(a, b, 0, EDGE.min(len))?
        || !range_eq(a, b, len.saturating_sub(EDGE).max(EDGE.min(len)), len)?
    {
        return Ok(false);
    }
    // The header words after magic and version: flags u32, toc_offset u64, toc_len u64.
    if len < 28 {
        return Ok(true);
    }
    let mut header = [0u8; 28];
    a.seek(SeekFrom::Start(0))?;
    a.read_exact(&mut header)?;
    let word = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap_or([0; 8]));
    let (start, toc_len) = (word(12), word(20));
    range_eq(a, b, start.min(len), start.saturating_add(toc_len).min(len))
}

/// Copies the permutation list out of the APK unless the same bytes are already there. The whole
/// file is compared: it is small, and keys of one length differ only in the middle, where
/// [`extract_pak`]'s edge check would miss them.
#[cfg(all(target_os = "android", feature = "client"))]
fn extract_permutation_list(
    mount: &regia_platform::ApkAssetMount,
    name: &str,
    dest: &std::path::Path,
) -> std::io::Result<()> {
    use regia_platform::VfsMount;

    let bytes = mount.read(name)?;
    if std::fs::read(dest).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dest)
}

/// Whether an APK assets-root file name is packaged media the video player reads by project path
/// (`.webm`/`.mkv` clips and their `.video.ron`/`.videolist.ron` sources, specs/50 § 4). Nothing
/// else in the engine opens these from the extracted content mount. Shared by the Android
/// extraction and `regia package`'s media index.
#[must_use]
pub fn is_packaged_media_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".webm")
        || lower.ends_with(".mkv")
        || lower.ends_with(".video.ron")
        || lower.ends_with(".videolist.ron")
}

/// Whether `path` is a [`PACKAGED_MEDIA_INDEX`] entry the extraction copies: a relative,
/// `/`-separated path of plain names that ends in a packaged-media file name
/// ([`is_packaged_media_name`]).
///
/// The index comes out of the package, so the runtime does not trust its paths. A `..`, `.` or
/// empty part, a leading `/`, a backslash, a `:` (a Windows drive or stream) and a NUL are
/// refused, so no entry names a file outside the extraction directory; so is a file the player
/// never reads by project path. `regia package` uses the same rule and leaves such a file out of
/// the index.
#[must_use]
pub fn is_safe_packaged_media_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains(['\0', '\\', ':']) {
        return false;
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    path.rsplit('/').next().is_some_and(is_packaged_media_name)
}

/// The name of the package-time index of raw media files, written to the package's assets root
/// by `regia package` and read by the Android runtime at startup: an APK assets directory
/// cannot be walked for subdirectories, so the extraction copies exactly the paths the index
/// lists (ADR-5441, amending ADR-4826's walk, which found nested media only when the mount
/// reported subdirectory names).
pub const PACKAGED_MEDIA_INDEX: &str = "media_index.ron";

/// One entry of [`PACKAGED_MEDIA_INDEX`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackagedMediaEntry {
    /// Path relative to the assets root (`movies/intro.webm`), `/`-separated.
    pub path: String,
    /// File length in bytes.
    pub len: u64,
    /// [`packaged_media_stamp`] of the file bytes.
    pub hash: u64,
}

/// The `media_index.ron` payload (format version 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackagedMediaIndex {
    /// Format version; 1 is the first.
    pub format_version: u32,
    /// The packaged media files, sorted by path for a deterministic package.
    pub files: Vec<PackagedMediaEntry>,
}

/// The cheap stamp of a packaged media file: FNV-1a over the bytes (textbook algorithm, ~1 byte
/// per step). Recorded at package time and memoized next to each extracted copy, so a launch
/// never re-reads a big clip just to check whether it changed.
#[must_use]
pub fn packaged_media_stamp(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Recursively copies the packaged media files under the APK assets mount `mount` (at `relative`,
/// `""` for the assets root) into `dest`, preserving the layout. Every other file (notices,
/// config, the `content/` pak directory) stays in the APK: `MediaFiles` never reads them by
/// project path. A directory entry is recognized by `VfsDirEntry::is_dir`, or — when the mount
/// cannot mark it, as the NDK asset directory lists entries without their kind — by "cannot be
/// opened as a file, but exists".
///
/// This is the fallback for packages built before [`PACKAGED_MEDIA_INDEX`] existed; a real APK
/// mount lists no subdirectories, so only top-level media is found. New packages are served by
/// [`extract_packaged_media_indexed`]. Returns (copied, unchanged).
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn extract_packaged_media(
    mount: &dyn regia_platform::VfsMount,
    relative: &str,
    dest: &std::path::Path,
) -> std::io::Result<(usize, usize)> {
    let entries = match mount.list_dir(relative) {
        Ok(entries) => entries,
        // No assets root in the APK (a hand-built dev APK): nothing to extract.
        Err(_) => return Ok((0, 0)),
    };
    let (mut copied, mut kept) = (0usize, 0usize);
    for entry in entries {
        let rel = if relative.is_empty() {
            entry.name.clone()
        } else {
            format!("{relative}/{}", entry.name)
        };
        let is_dir = entry.is_dir || (mount.open(&rel).is_err() && mount.exists(&rel));
        if is_dir {
            let (c, k) = extract_packaged_media(mount, &rel, &dest.join(&entry.name))?;
            copied += c;
            kept += k;
            continue;
        }
        if !is_packaged_media_name(&entry.name) {
            continue;
        }
        match extract_media_file(mount, &rel, &dest.join(&entry.name)) {
            Ok(true) => copied += 1,
            Ok(false) => kept += 1,
            // One bad clip is skipped: the others still extract.
            Err(err) => tracing::warn!(target: "regia", "cannot extract {rel}: {err}"),
        }
    }
    Ok((copied, kept))
}

/// Copies one packaged media file out of the APK unless an identical copy is already there (same
/// length, same bytes: media files are few, and reading an uncompressed APK asset is mmap-backed,
/// so a whole compare is cheap and a same-length edit is never missed). Streams through a
/// temporary file and a rename: a launch killed mid-copy never leaves a torn clip.
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn extract_media_file(
    mount: &dyn regia_platform::VfsMount,
    relative: &str,
    dest: &std::path::Path,
) -> std::io::Result<bool> {
    use std::io::{Seek, SeekFrom, Write};

    let mut src = mount.open(relative)?;
    let len = src.seek(SeekFrom::End(0))?;
    if let Ok(meta) = std::fs::metadata(dest)
        && meta.len() == len
    {
        let mut existing = std::fs::File::open(dest)?;
        if same_pak_bytes(&mut existing, src.as_mut(), len, false)? {
            return Ok(false);
        }
    }
    src.seek(SeekFrom::Start(0))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("media.tmp");
    let copied = (|| -> std::io::Result<()> {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        std::io::copy(&mut src, &mut out)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        std::fs::rename(&tmp, dest)
    })();
    if let Err(err) = copied {
        // A failed copy must not leave a torn temporary behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(true)
}

/// The stamp sidecar of an extracted media file: `intro.webm` extracts next to `intro.webm.stamp`
/// holding `"{len} {hash}"`. Reading it is the per-launch change check: when it matches the
/// index entry, neither the (deflated) APK asset nor the extracted copy is read at all.
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn media_stamp_path(dest: &std::path::Path) -> std::path::PathBuf {
    let Some(name) = dest.file_name().and_then(|n| n.to_str()) else {
        return dest.with_extension("stamp");
    };
    dest.with_file_name(format!("{name}.stamp"))
}

/// Whether the extracted copy at `dest` matches the index stamp: the sidecar exists and carries
/// exactly `len` and `hash`. A missing sidecar (a copy made before the stamps, or a user-serviced
/// file) reads as changed and is re-copied once, establishing the stamp.
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn media_stamp_matches(dest: &std::path::Path, len: u64, hash: u64) -> bool {
    std::fs::read_to_string(media_stamp_path(dest))
        .map(|text| text.trim() == format!("{len} {hash}"))
        .unwrap_or(false)
}

/// Writes the stamp sidecar of an extracted media file (tmp + rename, like the copy itself).
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn write_media_stamp(dest: &std::path::Path, len: u64, hash: u64) -> std::io::Result<()> {
    let stamp = media_stamp_path(dest);
    let tmp = stamp.with_extension("tmp");
    std::fs::write(&tmp, format!("{len} {hash}\n"))?;
    match std::fs::rename(&tmp, &stamp) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Copies one packaged media file like [`extract_media_file`], but the change check is the
/// package-time stamp, not a whole-byte compare: the (deflated) APK asset is opened only when a
/// copy is actually needed. Returns whether it copied.
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn extract_media_file_stamped(
    mount: &dyn regia_platform::VfsMount,
    relative: &str,
    dest: &std::path::Path,
    len: u64,
    hash: u64,
) -> std::io::Result<bool> {
    if media_stamp_matches(dest, len, hash) {
        return Ok(false);
    }
    let copied = extract_media_file(mount, relative, dest)?;
    write_media_stamp(dest, len, hash)?;
    Ok(copied)
}

/// Removes extracted media files (and their stamp sidecars) under `dest` whose assets-relative
/// `/`-separated path is not in `wanted`: an updated APK must not leave a renamed clip behind
/// forever, and a stale reference must fail with `source_not_found`, not open removed content.
/// Emptied directories go too.
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn prune_stale_media(dest: &std::path::Path, wanted: &std::collections::BTreeSet<String>) {
    fn walk(dir: &std::path::Path, root: &std::path::Path, wanted: &std::collections::BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&path, root, wanted);
                // Best effort: the dir may hold non-media files (the paks live here too).
                let _ = std::fs::remove_dir(&path);
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            // A stamp's media path is the stamp's own path minus the suffix; a stamp is stale
            // exactly when its media file is not wanted.
            let stale = if let Some(media_rel) = rel.strip_suffix(".stamp") {
                !wanted.contains(media_rel)
            } else {
                is_packaged_media_name(&name) && !wanted.contains(&rel)
            };
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    walk(dest, dest, wanted);
}

/// Extracts the packaged media into `dest` from the package-time [`PACKAGED_MEDIA_INDEX`], and
/// prunes extracted media the index no longer lists. Packages built before the index (or with an
/// unreadable/unsupported one) fall back to the [`extract_packaged_media`] walk, which finds only
/// top-level files through a real APK mount. Returns (copied, unchanged).
#[cfg(any(all(target_os = "android", feature = "client", feature = "media"), test))]
fn extract_packaged_media_indexed(
    mount: &dyn regia_platform::VfsMount,
    dest: &std::path::Path,
) -> std::io::Result<(usize, usize)> {
    let index: PackagedMediaIndex = match mount.read(PACKAGED_MEDIA_INDEX) {
        Ok(bytes) => match ron::from_str(&String::from_utf8_lossy(&bytes)) {
            Ok(index) => index,
            Err(err) => {
                tracing::warn!(target: "regia", "malformed {PACKAGED_MEDIA_INDEX} ({err}); walking the assets root instead");
                return extract_packaged_media(mount, "", dest);
            }
        },
        Err(_) => {
            // A package built before the index existed.
            return extract_packaged_media(mount, "", dest);
        }
    };
    if index.format_version != 1 {
        tracing::warn!(target: "regia", "unsupported {PACKAGED_MEDIA_INDEX} version {}; walking the assets root instead", index.format_version);
        return extract_packaged_media(mount, "", dest);
    }
    let mut wanted = std::collections::BTreeSet::new();
    let (mut copied, mut kept) = (0usize, 0usize);
    for entry in &index.files {
        // The index is package data: a path that could leave `dest`, or that is not media, is
        // not copied (and not kept by the prune below).
        if !is_safe_packaged_media_path(&entry.path) {
            tracing::warn!(
                target: "regia",
                "skipping {PACKAGED_MEDIA_INDEX} entry {:?}: not a relative media path",
                entry.path
            );
            continue;
        }
        wanted.insert(entry.path.clone());
        match extract_media_file_stamped(mount, &entry.path, &dest.join(&entry.path), entry.len, entry.hash) {
            Ok(true) => copied += 1,
            Ok(false) => kept += 1,
            // One bad clip is skipped: the others still extract.
            Err(err) => tracing::warn!(target: "regia", "cannot extract {}: {err}", entry.path),
        }
    }
    prune_stale_media(dest, &wanted);
    Ok((copied, kept))
}

/// The one frame profiler of the process (logging feeds it; the `Profiler` resource and the
/// telemetry hitch event read it). A host that installs its own `tracing` subscriber adds this as
/// a layer so hitch events name their top CPU spans: `registry().with(regia::frame_profiler())`.
#[must_use]
pub fn frame_profiler() -> regia_core::profiling::FrameProfiler {
    profiler()
}

fn profiler() -> regia_core::profiling::FrameProfiler {
    static PROFILER: std::sync::OnceLock<regia_core::profiling::FrameProfiler> = std::sync::OnceLock::new();
    PROFILER
        .get_or_init(regia_core::profiling::FrameProfiler::new)
        .clone()
}

/// The log setup: the session files go to `file_dir`, and `keep` (when set, from `server.toml`
/// `logs.keep_files`) is how many of them stay; at least one.
fn log_config(file_dir: Option<std::path::PathBuf>, keep: Option<u32>) -> regia_core::log::LogConfig {
    let mut config = regia_core::log::LogConfig {
        file_dir,
        profiler: Some(profiler()),
        ..Default::default()
    };
    if let Some(keep) = keep {
        config.max_files = (keep as usize).max(1);
    }
    config
}

fn start_logging(
    file_dir: Option<std::path::PathBuf>,
    keep: Option<u32>,
) -> Option<regia_core::log::LogGuard> {
    let extra = json_log_layer();
    let config = log_config(file_dir, keep);
    match regia_core::log::init_with_layer(config, extra) {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!("regia: logging setup failed: {err}");
            flush_json_log();
            None
        }
    }
}

/// JSON-lines log requested by [`regia_core::log::JSON_LOG_ENV`]. A missing parent directory or a
/// file that cannot be created warns and continues with no JSON log. Shipping builds honor it too.
#[cfg(feature = "client")]
fn json_log_layer() -> Option<regia_core::log::ExtraLogLayer> {
    let Ok(path_text) = regia_core::brand::env_var(regia_core::log::JSON_LOG_ENV) else {
        return None;
    };
    let path_text = path_text.trim();
    if path_text.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(path_text);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "regia: {} directory {} could not be created: {err}",
            regia_core::log::JSON_LOG_ENV,
            parent.display()
        );
        return None;
    }
    let mut options = regia_telemetry::JsonLayerOptions::default();
    // `REGIA_TELEMETRY=ship` filters the file too (ADR-8417); the other levels leave the
    // JSON log as it was (it writes what the host's filter passes).
    if regia_telemetry::TelemetryLevel::from_env() == Some(regia_telemetry::TelemetryLevel::Ship) {
        options.level = regia_telemetry::TelemetryLevel::Ship;
    }
    match regia_telemetry::json_layer(&path, options) {
        Ok((layer, guard)) => {
            store_json_guard(guard);
            Some(regia_core::log::box_extra_layer(layer))
        }
        Err(err) => {
            eprintln!(
                "regia: {} {} could not be opened: {err}",
                regia_core::log::JSON_LOG_ENV,
                path.display()
            );
            None
        }
    }
}

#[cfg(not(feature = "client"))]
fn json_log_layer() -> Option<regia_core::log::ExtraLogLayer> {
    None
}

#[cfg(feature = "client")]
fn json_guard_slot() -> &'static std::sync::Mutex<Option<regia_telemetry::JsonLayerGuard>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<regia_telemetry::JsonLayerGuard>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(feature = "client")]
fn store_json_guard(guard: regia_telemetry::JsonLayerGuard) {
    match json_guard_slot().lock() {
        Ok(mut slot) => *slot = Some(guard),
        Err(poisoned) => *poisoned.into_inner() = Some(guard),
    }
}

/// Drops the JSON-log guard so its writer thread flushes. `finish_exit` can end the process
/// without running static destructors, so both exit paths call this explicitly.
fn flush_json_log() {
    #[cfg(feature = "client")]
    {
        let taken = match json_guard_slot().lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(taken);
    }
}

/// `true` when `regia` is built with its `server` feature, so [`run_with`] hosts a dedicated server
/// instead of opening a window (ADR-3075). Cargo unifies features across every package of one
/// build, so a game client built in the same cargo invocation as its server (`cargo build -p a_server
/// -p a_client`, or `--workspace`) is a server build too. A client checks this before
/// [`run_with`] and refuses to start with a clear message.
pub const SERVER_BUILD: bool = cfg!(feature = "server");

/// Runs the game: logging, the engine plugins, the game's plugin(s), then the host loop.
/// `configure` may adjust the app before it runs.
#[cfg_attr(
    not(feature = "server"),
    doc = "On a client build this opens a window ([`DefaultPlugins`], then the platform runner).\n\nAfter a deliberate exit (a lost GPU device: exit code 3, no window surface: 4; ADR-276) this does not return: once the app has exited and dropped, the log is flushed and the process ends with that code without running process-exit handlers, which can crash in a GPU driver whose device is already lost ([`regia_platform::finish_deliberate_exit`]).\n\nAfter any other exit it waits for the engine's GPU worker threads (ADR-287, [`regia_platform::finish_exit`]) and returns once they have ended. If one is still running after the shared budget (30 s from the start of the exit, `REGIA_EXIT_WORKER_WAIT_MS`), it may be inside the GPU driver: the process then ends with the app's exit code the same way, without running process-exit handlers."
)]
#[cfg_attr(
    feature = "server",
    doc = "With the `server` feature this is the dedicated-server host (`server::run_with`): no window and no GPU-worker wait. See [`SERVER_BUILD`].\n\nExit codes follow spec 76 § 5 and `server_host::ExitCode`: configuration 2, a port that could not be bound 3, content 4, crash 70, success 0. A refused configuration, bind or content error ends the process with that code (`process::exit`)."
)]
///
/// ```no_run
/// use regia::prelude::*;
///
/// struct Empty;
/// impl Plugin for Empty {
///     fn build(&self, _app: &mut App) {}
/// }
///
/// // Adjust the app before it runs: here, the window title.
/// let exit = regia::run_with(Empty, |app| {
///     app.insert_resource(WindowSettings {
///         window: Window {
///             title: "My game".into(),
///             ..Default::default()
///         },
///     });
/// });
/// assert!(matches!(exit, AppExit::Success));
/// ```
pub fn run_with(game: impl Plugins, configure: impl FnOnce(&mut App)) -> AppExit {
    #[cfg(feature = "server")]
    {
        server::run_with(game, configure)
    }
    #[cfg(not(feature = "server"))]
    {
        run_client(game, configure)
    }
}

/// The client side of [`run_with`].
#[cfg(not(feature = "server"))]
fn run_client(game: impl Plugins, configure: impl FnOnce(&mut App)) -> AppExit {
    #[cfg(not(target_os = "android"))]
    let _log = start_logging(regia_platform::paths::default_log_dir(), None);
    #[cfg(target_os = "android")]
    let _log = start_logging(None, None);
    let mut app = App::new();
    app.insert_resource(Profiler(profiler()));
    app.add_plugins(DefaultPlugins).add_plugins(game);
    #[cfg(all(feature = "net", feature = "client"))]
    dev_client::install(&mut app);
    configure(&mut app);
    let exit = app.run();
    regia_platform::finish_exit(exit_code(&exit), || {
        if let Some(log) = &_log {
            log.flush();
        }
        flush_json_log();
    });
    exit
}

/// The process exit code of `exit` (what returning it from `main` reports).
#[cfg(not(feature = "server"))]
fn exit_code(exit: &AppExit) -> i32 {
    match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    }
}

/// Runs a desktop game with default settings. See [`run_with`].
///
/// ```no_run
/// use regia::prelude::*;
///
/// struct Empty;
/// impl Plugin for Empty {
///     fn build(&self, _app: &mut App) {}
/// }
///
/// // Blocks until the window closes, then reports how the app ended.
/// let exit = regia::run(Empty);
/// assert!(matches!(exit, AppExit::Success));
/// ```
pub fn run(game: impl Plugins) -> AppExit {
    run_with(game, |_| {})
}

/// Runs a game on Android. Called by the function [`android_main!`] generates. Never returns: the
/// process ends when the game exits (ADR-013).
#[cfg(all(target_os = "android", feature = "client"))]
pub fn run_android(android: regia_platform::AndroidApp, game: impl Plugins) {
    let _log = start_logging(regia_platform::paths::default_log_dir(&android), None);
    let mut app = App::new();
    app.insert_resource(regia_platform::AndroidAppHandle(android));
    app.insert_resource(Profiler(profiler()));
    app.add_plugins(DefaultPlugins).add_plugins(game);
    let exit = app.run();
    tracing::info!(target: "regia", "game exited ({exit:?}); ending the process");
    if let Some(log) = &_log {
        log.flush();
    }
    flush_json_log();
    // The process must end with the game (one winit event loop per process); see `exit_process`.
    regia_platform::exit_process(match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    })
}

/// Generates the `android_main` entry point of an Android `cdylib` crate (lib name `main`):
/// `regia::android_main!(my_game::MyGame);`. Expands to nothing on other platforms.
///
/// The entry point needs `#[unsafe(no_mangle)]`; the macro allows `unsafe_code` on that one item so
/// the calling crate keeps the workspace's `unsafe_code = "deny"` (ADR-012 § 5).
#[macro_export]
macro_rules! android_main {
    ($game:expr) => {
        #[cfg(target_os = "android")]
        #[allow(unsafe_code)]
        // SAFETY: `android-activity` calls this one symbol, with this signature. A second definition
        // in the same library is a link error, not a silent replacement.
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::platform::AndroidApp) {
            $crate::run_android(app, $game);
        }
    };
}

/// `regia run --level L` replaces whatever level the game opened, so `Baked` generators read the
/// bakes beside `L` (spec 62 § 8). Runs after [`regia_framework::start_level::open_start_level`]:
/// `opened` is set only when that open succeeds, and only while that level is still current.
///
/// The directory is applied once per open (tracked in `applied`), not every frame: while the
/// start level stays current a host write such as `regia_procgen::world::follow_level(world,
/// None)` used to be undone on the next frame (corr-low, ADR-5789).
#[cfg(feature = "procgen")]
fn start_level_bake_dir(
    start: Option<Res<regia_framework::start_level::StartLevel>>,
    levels: Option<Res<regia_scene::level::LevelManager>>,
    settings: Option<ResMut<regia_procgen::world::ProcGenSettings>>,
    mut applied: Local<Option<(regia_scene::level::LevelId, std::path::PathBuf)>>,
) {
    let (Some(start), Some(levels), Some(mut settings)) = (start, levels, settings) else {
        return;
    };
    // A failed read, parse or spawn clears `dirty` and leaves `opened` unchanged. A later level
    // change can leave the old id in `opened` while another level is current.
    let Some(opened) = start.opened else {
        return;
    };
    if levels.current() != Some(opened) {
        return;
    }
    let dir = regia_procgen::world::level_bake_dir(&start.path);
    if applied
        .as_ref()
        .is_some_and(|(id, path)| *id == opened && *path == dir)
    {
        return;
    }
    if settings.bake_dir.as_ref() != Some(&dir) {
        settings.bake_dir = Some(dir.clone());
    }
    *applied = Some((opened, dir));
}

/// A level opened by path after the start (a level change: `load_level_source` then a `Replace`
/// `open_level`) names the bake directory beside it, so its `Baked` generators read their bakes
/// (spec 62 § 8, ADR-2421). An additive load keeps the primary level's directory.
///
/// `open_level` writes `LevelLoading` before validation and spawn. The directory moves only when
/// that `Replace` also wrote `LevelLoaded`, which happens only after the level is open. A path
/// load inserts the level first, so both messages from one open arrive in the same read.
#[cfg(feature = "procgen")]
fn level_change_bake_dir(
    mut loading: MessageReader<regia_scene::level::LevelLoading>,
    mut loaded: MessageReader<regia_scene::level::LevelLoaded>,
    sources: Option<Res<regia_framework::level_assets::LoadedLevelSources>>,
    settings: Option<ResMut<regia_procgen::world::ProcGenSettings>>,
) {
    let opened: Vec<_> = loaded.read().map(|message| message.level).collect();
    let Some(level) = loading
        .read()
        .filter(|message| {
            message.mode == regia_scene::level::LoadMode::Replace && opened.contains(&message.level)
        })
        .last()
        .map(|message| message.level)
    else {
        return;
    };
    let (Some(sources), Some(mut settings)) = (sources, settings) else {
        return;
    };
    let Some(path) = sources
        .levels
        .iter()
        .find(|(_, id)| **id == level)
        .map(|(p, _)| p)
    else {
        return;
    };
    let dir = regia_procgen::world::level_bake_dir(path);
    if settings.bake_dir.as_ref() != Some(&dir) {
        settings.bake_dir = Some(dir);
    }
}

/// Every level `load_level_source` read (the opened one and the levels it places, transitively)
/// names its bake directory in `ProcLevelBakeDirs`, so a generator a placed or additive level
/// brought in loads the bake beside its own level (spec 62 § 8, ADR-7262). Runs when the loaded
/// sources change; entries are only added or updated.
#[cfg(feature = "procgen")]
fn level_bake_dirs(
    sources: Option<Res<regia_framework::level_assets::LoadedLevelSources>>,
    dirs: Option<ResMut<regia_procgen::world::ProcLevelBakeDirs>>,
) {
    let (Some(sources), Some(mut dirs)) = (sources, dirs) else {
        return;
    };
    if !sources.is_changed() {
        return;
    }
    for (path, id) in &sources.levels {
        let dir = regia_procgen::world::level_bake_dir(path);
        if dirs.0.get(id) != Some(&dir) {
            dirs.0.insert(*id, dir);
        }
    }
}

#[cfg(all(test, feature = "procgen"))]
mod tests {
    use std::path::{Path, PathBuf};

    use regia_app::prelude::*;
    use regia_framework::start_level::StartLevel;
    use regia_procgen::world::ProcGenSettings;

    use super::{level_bake_dirs, level_change_bake_dir, same_pak_bytes, start_level_bake_dir};

    /// Every loaded level source names its bake directory, placed sublevels included (ADR-7262).
    #[test]
    fn loaded_level_sources_name_their_bake_directories() {
        use regia_core::id::Guid;
        use regia_procgen::world::ProcLevelBakeDirs;
        use regia_scene::level::LevelId;
        let (main, sub) = (LevelId(Guid(1)), LevelId(Guid(2)));
        let mut app = App::new();
        app.init_resource::<ProcLevelBakeDirs>()
            .add_systems(Update, level_bake_dirs);
        app.update();
        assert!(app.world().resource::<ProcLevelBakeDirs>().0.is_empty());
        let mut sources = regia_framework::level_assets::LoadedLevelSources::default();
        sources
            .levels
            .insert(PathBuf::from("assets/maps/main.level.ron"), main);
        sources
            .levels
            .insert(PathBuf::from("assets/maps/rooms/sub.level.d"), sub);
        app.insert_resource(sources);
        app.update();
        let dirs = &app.world().resource::<ProcLevelBakeDirs>().0;
        assert_eq!(
            dirs.get(&main).map(PathBuf::as_path),
            Some(Path::new("assets/maps/main.procgen"))
        );
        assert_eq!(
            dirs.get(&sub).map(PathBuf::as_path),
            Some(Path::new("assets/maps/rooms/sub.procgen"))
        );
    }

    /// A level change moves the bake directory to the new level's; an additive load does not.
    #[test]
    fn a_level_change_names_the_new_bake_directory() {
        use regia_core::id::Guid;
        use regia_scene::level::{LevelId, LevelLoaded, LevelLoading, LoadMode};
        let (first, second) = (LevelId(Guid(1)), LevelId(Guid(2)));
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            bake_dir: Some(PathBuf::from("assets/maps/start.procgen")),
            ..ProcGenSettings::default()
        });
        let mut sources = regia_framework::level_assets::LoadedLevelSources::default();
        sources
            .levels
            .insert(PathBuf::from("assets/maps/cave.level.ron"), first);
        sources
            .levels
            .insert(PathBuf::from("assets/maps/town.level.d"), second);
        app.insert_resource(sources)
            .add_message::<LevelLoading>()
            .add_message::<LevelLoaded>()
            .add_systems(Update, level_change_bake_dir);
        let dir = |app: &App| app.world().resource::<ProcGenSettings>().bake_dir.clone();
        let spawn = |app: &mut App, level: LevelId, mode: LoadMode| {
            app.world_mut().write_message(LevelLoading { level, mode });
            // `LevelLoaded` is what a spawn that finished writes. A failed open writes only
            // `LevelLoading`.
            app.world_mut().write_message(LevelLoaded { level, entities: 1 });
        };
        spawn(&mut app, first, LoadMode::Replace);
        app.update();
        assert_eq!(dir(&app).as_deref(), Some(Path::new("assets/maps/cave.procgen")));
        spawn(&mut app, second, LoadMode::Additive);
        app.update();
        assert_eq!(
            dir(&app).as_deref(),
            Some(Path::new("assets/maps/cave.procgen")),
            "additive"
        );
        spawn(&mut app, second, LoadMode::Replace);
        app.update();
        assert_eq!(dir(&app).as_deref(), Some(Path::new("assets/maps/town.procgen")));
    }

    /// `open_level` writes `LevelLoading` before validation. A replace that fails leaves the
    /// world on the level that was already open, and the bake directory stays with that level.
    #[test]
    fn a_failed_replace_keeps_the_open_levels_bake_directory() {
        use regia_core::id::Guid;
        use regia_scene::format::{SceneEntity, SceneFile};
        use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            bake_dir: Some(PathBuf::from("assets/maps/cave.procgen")),
            ..ProcGenSettings::default()
        });
        let cave = LevelId(Guid::from_u128(1));
        let town = LevelId(Guid::from_u128(2));
        let shared = Guid::from_u128(0x8b6f_2a11);
        let mut duplicate = SceneFile::new();
        duplicate.entities.push(SceneEntity {
            guid: shared,
            name: Some("a".to_string()),
            ..SceneEntity::default()
        });
        duplicate.entities.push(SceneEntity {
            guid: shared,
            name: Some("b".to_string()),
            ..SceneEntity::default()
        });
        {
            let mut library = app.world_mut().resource_mut::<LevelLibrary>();
            library.insert(Level::from_scene_file(cave, SceneFile::new()));
            library.insert(Level::from_scene_file(town, duplicate));
        }
        open_level(app.world_mut(), cave, LoadMode::Replace).expect("cave opens");
        let mut sources = regia_framework::level_assets::LoadedLevelSources::default();
        sources
            .levels
            .insert(PathBuf::from("assets/maps/cave.level.ron"), cave);
        sources
            .levels
            .insert(PathBuf::from("assets/maps/town.level.ron"), town);
        app.insert_resource(sources)
            .add_systems(Update, level_change_bake_dir);
        let err = open_level(app.world_mut(), town, LoadMode::Replace).expect_err("duplicate guids");
        assert!(
            err.to_string().contains("duplicate entity guids"),
            "validation must refuse the file: {err}"
        );
        app.update();
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(Path::new("assets/maps/cave.procgen")),
            "a failed replace must not point bakes at the level that did not open"
        );
        assert_eq!(
            app.world()
                .resource::<regia_scene::level::LevelManager>()
                .current(),
            Some(cave)
        );
    }

    fn bake_dir_after_a_frame(start: Option<&str>) -> Option<PathBuf> {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            bake_dir: Some(PathBuf::from("game/choice.procgen")),
            ..ProcGenSettings::default()
        });
        if let Some(path) = start {
            app.insert_resource(StartLevel::new(path));
        }
        app.add_systems(Update, start_level_bake_dir);
        app.update();
        app.world().resource::<ProcGenSettings>().bake_dir.clone()
    }

    #[test]
    fn the_start_level_names_the_bake_directory() {
        let dir = regia_test_utils::unique_temp_dir("start_level_bake_ok");
        let forest = dir.join("forest.level.ron");
        regia_test_utils::write_atomic(&forest, b"(format_version: 1, entities: [])").expect("write");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            bake_dir: Some(PathBuf::from("assets/maps/cave.procgen")),
            ..ProcGenSettings::default()
        });
        app.insert_resource(StartLevel::new(&forest));
        // The shipped order: the open, then the directory of the level that opened.
        app.add_systems(
            Update,
            (
                regia_framework::start_level::open_start_level,
                start_level_bake_dir,
            )
                .chain(),
        );
        app.update();
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(regia_procgen::world::level_bake_dir(&forest).as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-low: the start-level system ran every frame, so while the start level stayed current
    /// it set `bake_dir` back on the frame after a host cleared it (a
    /// `regia_procgen::world::follow_level(world, None)`). The directory names the open once;
    /// a later host write stands.
    #[test]
    fn a_host_clear_of_the_bake_dir_is_not_undone_while_the_start_level_is_current() {
        let dir = regia_test_utils::unique_temp_dir("start_level_bake_host_clear");
        let forest = dir.join("forest.level.ron");
        regia_test_utils::write_atomic(&forest, b"(format_version: 1, entities: [])").expect("write");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings::default());
        app.insert_resource(StartLevel::new(&forest));
        app.add_systems(
            Update,
            (
                regia_framework::start_level::open_start_level,
                start_level_bake_dir,
            )
                .chain(),
        );
        app.update();
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(regia_procgen::world::level_bake_dir(&forest).as_path()),
            "precondition: the open named the directory"
        );
        // The host points bakes elsewhere (or nowhere); the next frames must not revert it.
        regia_procgen::world::follow_level(app.world_mut(), None);
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir,
            None,
            "a host write stands while the start level stays current"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cave is open and its bake directory is set. Forest, the start level, is marked dirty and
    /// its file does not parse. The open fails, cave stays, and bakes stay on cave.
    #[test]
    fn a_failed_start_level_reload_keeps_the_open_bake_directory() {
        use regia_core::id::Guid;
        use regia_scene::format::SceneFile;
        use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

        let dir = regia_test_utils::unique_temp_dir("start_level_bake_fail");
        let forest = dir.join("forest.level.ron");
        regia_test_utils::write_atomic(&forest, b"not a level").expect("truncated");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let cave = LevelId(Guid::from_u128(0xCA5E));
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(cave, SceneFile::new()));
        open_level(app.world_mut(), cave, LoadMode::Replace).expect("cave opens");
        let mut start = StartLevel::new(&forest);
        // The earlier successful open of forest; a later level change left cave current.
        start.opened = Some(regia_framework::level_assets::level_id_of(&forest));
        start.dirty = true;
        app.insert_resource(start);
        app.insert_resource(ProcGenSettings {
            bake_dir: Some(PathBuf::from("assets/maps/cave.procgen")),
            ..ProcGenSettings::default()
        });
        // Runs while the start level is still dirty, which is when the old schedule moved the
        // directory, before the open had a chance to fail.
        app.add_systems(Update, start_level_bake_dir);
        app.update();
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(Path::new("assets/maps/cave.procgen")),
            "a dirty start level that is not the open level must not move the bake directory"
        );
        regia_framework::start_level::open_start_level(app.world_mut());
        assert_eq!(
            app.world()
                .resource::<regia_scene::level::LevelManager>()
                .current(),
            Some(cave),
            "the truncated file must not replace cave"
        );
        app.update();
        assert_eq!(
            app.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(Path::new("assets/maps/cave.procgen"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_start_level_the_game_keeps_its_own() {
        assert_eq!(
            bake_dir_after_a_frame(None).as_deref(),
            Some(Path::new("game/choice.procgen"))
        );
    }

    /// A fake FPAK: header words, then filler. `toc_offset`/`toc_len` name a region in the middle.
    fn fake_pak(len: usize, toc_offset: u64, toc_len: u64) -> Vec<u8> {
        let mut bytes = vec![7u8; len];
        bytes[..4].copy_from_slice(b"FPAK");
        bytes[12..20].copy_from_slice(&toc_offset.to_le_bytes());
        bytes[20..28].copy_from_slice(&toc_len.to_le_bytes());
        bytes
    }

    fn same(a: &[u8], b: &[u8], is_pak: bool) -> bool {
        same_pak_bytes(
            &mut std::io::Cursor::new(a),
            &mut std::io::Cursor::new(b),
            a.len() as u64,
            is_pak,
        )
        .expect("reads")
    }

    #[test]
    fn a_pak_toc_change_outside_the_edges_is_seen() {
        // TOC at 20_000..30_000 of a 40_000-byte pak: well outside the first and last 4 KiB.
        let old = fake_pak(40_000, 20_000, 10_000);
        let mut new = old.clone();
        assert!(same(&old, &new, true));
        new[25_000] ^= 1;
        assert!(!same(&old, &new, true), "a middle TOC byte differs");
        // Payload bytes outside the TOC are covered by the header's hash, not read here.
        let mut payload = old.clone();
        payload[10_000] ^= 1;
        assert!(same(&old, &payload, true));
    }

    #[test]
    fn a_manifest_compares_whole() {
        let old = vec![1u8; 20_000];
        let mut new = old.clone();
        new[10_000] = 2;
        assert!(!same(&old, &new, false));
        assert!(same(&old, &old.clone(), false));
    }

    #[test]
    fn a_bad_toc_range_does_not_read_past_the_file() {
        let old = fake_pak(100, u64::MAX - 5, u64::MAX);
        assert!(same(&old, &old.clone(), true));
    }
}

/// Media audit G finding 1: packaged media extraction for Android (`is_packaged_media_name`,
/// `extract_packaged_media`, `extract_media_file`). A `DirMount` stands in for the APK assets
/// mount; the walker must also handle the NDK listing entries without their kind (a directory
/// that cannot be opened as a file but exists).
#[cfg(test)]
mod media_mount_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use regia_platform::{DirMount, VfsMount};

    use super::{
        PACKAGED_MEDIA_INDEX, PackagedMediaEntry, PackagedMediaIndex, extract_packaged_media,
        extract_packaged_media_indexed, is_packaged_media_name, is_safe_packaged_media_path,
        packaged_media_stamp,
    };

    fn file_name(rel: &str) -> &str {
        Path::new(rel).file_name().and_then(|n| n.to_str()).expect("name")
    }

    /// A fake APK assets root: media at the top level and in nested directories, plus the files
    /// that must stay in the APK.
    fn apk_assets() -> (PathBuf, Vec<(&'static str, Vec<u8>)>) {
        let root = regia_test_utils::unique_temp_dir("apk_assets");
        let clip_a = vec![0x1a, 0x45, 0xdf, 0xa3, 1, 2, 3];
        let clip_b = vec![0x1a, 0x45, 0xdf, 0xa3, 4, 5];
        let cut = vec![0x1a, 0x45, 0xdf, 0xa3, 6];
        let files = vec![
            ("movies/intro.webm", clip_a.clone()),
            (
                "movies/intro.video.ron",
                br#"(format_version: 1, file: "movies/intro.webm")"#.to_vec(),
            ),
            ("movies/deep/cut.mkv", cut.clone()),
            (
                "movies/deep/all.videolist.ron",
                b"(format_version: 1, entries: [])".to_vec(),
            ),
            ("top.webm", clip_b.clone()),
            ("about.txt", b"hello".to_vec()),
            ("content/base.fpak", vec![7u8; 64]),
            ("THIRD_PARTY_NOTICES.md", b"notices".to_vec()),
        ];
        for (rel, bytes) in &files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, bytes).expect("file");
        }
        (root, files)
    }

    #[test]
    fn the_packaged_media_list_covers_player_sources() {
        for name in [
            "intro.webm",
            "cut.mkv",
            "intro.video.ron",
            "all.videolist.ron",
            "nested/dir/intro.WEBM",
        ] {
            assert!(is_packaged_media_name(name), "{name}");
        }
        for name in [
            "base.fpak",
            "net-manifest.ron",
            "about.txt",
            "permutations.json",
            "intro.webm.meta",
            "movies",
        ] {
            assert!(!is_packaged_media_name(name), "{name}");
        }
    }

    #[test]
    fn extraction_pulls_only_media_and_preserves_the_layout() {
        let (root, files) = apk_assets();
        let mount = DirMount::read_only(&root);
        let dest = regia_test_utils::unique_temp_dir("extracted_media");
        let (copied, kept) = extract_packaged_media(&mount, "", &dest).expect("extract");
        assert_eq!(copied, 5, "five media files: {copied}");
        assert_eq!(kept, 0);
        for (rel, bytes) in &files {
            let at = dest.join(rel);
            if is_packaged_media_name(Path::new(rel).file_name().and_then(|n| n.to_str()).expect("name")) {
                assert_eq!(
                    std::fs::read(at).unwrap_or_else(|e| panic!("extracted {rel}: {e}")),
                    *bytes,
                    "{rel}"
                );
            } else {
                assert!(!at.exists(), "{rel} stays in the APK");
            }
        }
        // A second launch finds identical copies and rewrites nothing.
        let (copied, kept) = extract_packaged_media(&mount, "", &dest).expect("re-extract");
        assert_eq!(copied, 0, "unchanged clips are kept");
        assert_eq!(kept, 5);
        // An edited clip (same length) is re-copied: the whole-byte compare misses nothing.
        std::fs::write(root.join("top.webm"), vec![9u8; 6]).expect("edit");
        let (copied, kept) = extract_packaged_media(&mount, "", &dest).expect("re-extract after edit");
        assert_eq!(copied, 1);
        assert_eq!(kept, 4);
        assert_eq!(std::fs::read(dest.join("top.webm")).expect("top"), vec![9u8; 6]);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn the_index_drives_extraction_through_a_real_ndk_mount() {
        /// A mount that models the real NDK asset directory: `list_dir` lists the files directly
        /// in a directory, never its subdirectories, and entries carry no kind. Counts `open`
        /// calls so a test can prove a stamped re-extraction never reads a clip.
        struct NdkLike {
            inner: DirMount,
            opens: Arc<AtomicU64>,
        }
        impl VfsMount for NdkLike {
            fn read(&self, relative: &str) -> std::io::Result<Vec<u8>> {
                self.inner.read(relative)
            }
            fn open(&self, relative: &str) -> std::io::Result<Box<dyn regia_platform::VfsReader>> {
                self.opens.fetch_add(1, Ordering::Relaxed);
                self.inner.open(relative)
            }
            fn exists(&self, relative: &str) -> bool {
                self.inner.exists(relative)
            }
            fn list_dir(&self, relative: &str) -> std::io::Result<Vec<regia_platform::VfsDirEntry>> {
                // The real NDK lists only the files of a directory, not its subdirectories.
                Ok(self
                    .inner
                    .list_dir(relative)?
                    .into_iter()
                    .filter(|e| !e.is_dir)
                    .map(|mut e| {
                        e.is_dir = false;
                        e
                    })
                    .collect())
            }
            fn describe(&self) -> String {
                self.inner.describe()
            }
        }

        /// Writes a package-time index for `entries` (assets-root-relative paths) into the mount.
        fn write_index(root: &Path, entries: &[&str]) {
            let files = entries
                .iter()
                .map(|path| {
                    let bytes = std::fs::read(root.join(path)).expect("read media");
                    PackagedMediaEntry {
                        path: (*path).to_string(),
                        len: bytes.len() as u64,
                        hash: packaged_media_stamp(&bytes),
                    }
                })
                .collect();
            let index = PackagedMediaIndex {
                format_version: 1,
                files,
            };
            let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty()).expect("index ron");
            std::fs::write(root.join(PACKAGED_MEDIA_INDEX), text).expect("write index");
        }

        let (root, files) = apk_assets();
        let rels: Vec<&str> = files
            .iter()
            .map(|(rel, _)| *rel)
            .filter(|rel| is_packaged_media_name(file_name(rel)))
            .collect();
        write_index(&root, &rels);
        let opens = Arc::new(AtomicU64::new(0));
        let mount = NdkLike {
            inner: DirMount::read_only(&root),
            opens: Arc::clone(&opens),
        };
        let dest = regia_test_utils::unique_temp_dir("extracted_media_indexed");

        // First launch: every indexed file extracts, nested paths included, although the mount
        // cannot see a single subdirectory.
        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("extract");
        assert_eq!((copied, kept), (5, 0), "nested media extracts by index");
        for (rel, bytes) in &files {
            if is_packaged_media_name(file_name(rel)) {
                assert_eq!(
                    std::fs::read(dest.join(rel)).unwrap_or_else(|e| panic!("extracted {rel}: {e}")),
                    *bytes,
                    "{rel}"
                );
            }
        }

        // Second launch: the stamps match, so nothing is opened at all — no APK asset, no copy.
        let opened = opens.load(Ordering::Relaxed);
        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("re-extract");
        assert_eq!((copied, kept), (0, 5));
        assert_eq!(
            opens.load(Ordering::Relaxed),
            opened,
            "unchanged clips are not opened on a cold start"
        );

        // An update that drops a clip prunes it (and its stamp, and the emptied directory).
        std::fs::remove_file(root.join("movies/deep/cut.mkv")).expect("drop the clip");
        write_index(
            &root,
            &[
                "movies/intro.webm",
                "movies/intro.video.ron",
                "movies/deep/all.videolist.ron",
                "top.webm",
            ],
        );
        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("re-extract after update");
        assert_eq!((copied, kept), (0, 4));
        assert!(
            !dest.join("movies/deep/cut.mkv").exists(),
            "the stale clip is pruned"
        );
        assert!(
            !dest.join("movies/deep/cut.mkv.stamp").exists(),
            "the stale stamp is pruned"
        );
        assert!(
            dest.join("movies/deep").is_dir(),
            "the directory survives: it still holds the listed clip"
        );
        assert!(
            dest.join("movies/deep/all.videolist.ron").is_file(),
            "a listed nested clip survives the prune"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }

    /// An index that lists nothing says "this package ships no media". It is not the same as no
    /// index, which says "this package was built before the index existed": the first copies
    /// nothing and prunes what an earlier package extracted, the second walks and prunes nothing.
    /// v047's text index had this test (`an_empty_media_index_copies_nothing`); the integ-v048
    /// resolution dropped it with the text format, and with it the packager rule that an Android
    /// package always carries an index.
    #[test]
    fn an_empty_index_copies_nothing_and_prunes_what_an_earlier_package_extracted() {
        let (root, _files) = apk_assets();
        let mount = DirMount::read_only(&root);
        let dest = regia_test_utils::unique_temp_dir("extracted_media_empty_index");
        // An earlier version of the app extracted a clip it no longer ships.
        std::fs::create_dir_all(dest.join("movies")).expect("dir");
        std::fs::write(dest.join("movies/old.webm"), b"old clip").expect("old clip");
        std::fs::write(dest.join("movies/old.webm.stamp"), b"8 0").expect("old stamp");

        // No index: the walk copies what a directory mount lists and leaves the old clip.
        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("walk");
        assert_eq!(
            (copied, kept),
            (5, 0),
            "the walk of a directory mount finds every clip"
        );
        assert!(
            dest.join("movies/old.webm").is_file(),
            "no index: nothing is pruned"
        );

        // An empty index: nothing is copied, and everything extracted so far goes.
        let index = PackagedMediaIndex {
            format_version: 1,
            files: Vec::new(),
        };
        let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty()).expect("index ron");
        std::fs::write(root.join(PACKAGED_MEDIA_INDEX), text).expect("write index");
        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("empty index");
        assert_eq!(
            (copied, kept),
            (0, 0),
            "an empty index copies nothing and does not walk"
        );
        assert!(
            !dest.join("movies/old.webm").exists(),
            "the earlier clip is pruned"
        );
        assert!(!dest.join("movies/old.webm.stamp").exists(), "and its stamp");
        assert!(!dest.join("top.webm").exists(), "and what the walk had copied");
        assert!(!dest.join("movies/intro.webm").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn the_index_path_rule_takes_plain_relative_media_paths_only() {
        for path in [
            "top.webm",
            "movies/intro.webm",
            "movies/deep/cut.MKV",
            "movies/intro.video.ron",
            "a b/all.videolist.ron",
            "..movies/intro.webm",
            "movies./intro.webm",
        ] {
            assert!(is_safe_packaged_media_path(path), "{path:?}");
        }
        for path in [
            "",
            "/top.webm",
            "/abs/top.webm",
            "../top.webm",
            "movies/../top.webm",
            "movies/..",
            "./top.webm",
            "movies/./top.webm",
            "movies//top.webm",
            "movies/",
            "movies\\top.webm",
            "..\\top.webm",
            "C:top.webm",
            "C:/movies/top.webm",
            "movies/top.webm\0",
            "movies/about.txt",
            "content/base.fpak",
            "movies",
            PACKAGED_MEDIA_INDEX,
        ] {
            assert!(!is_safe_packaged_media_path(path), "{path:?}");
        }
    }

    /// The index comes out of the package, so its paths are untrusted. An entry that is not a
    /// plain relative media path (`..`, an absolute path, a backslash or drive form, an empty
    /// part, a file the player never reads) is skipped, and nothing is written outside the
    /// destination. v047's text index refused these lines (audfix-regia-g); the RON index that
    /// replaced it in integ-v048 joined every path as written.
    #[test]
    fn an_index_entry_that_is_not_a_plain_media_path_is_skipped() {
        /// A hostile package: every path its index names opens as a clip.
        struct AnyPath(DirMount);
        impl VfsMount for AnyPath {
            fn read(&self, relative: &str) -> std::io::Result<Vec<u8>> {
                if relative == PACKAGED_MEDIA_INDEX {
                    self.0.read(relative)
                } else {
                    self.0.read("top.webm")
                }
            }
            fn open(&self, _relative: &str) -> std::io::Result<Box<dyn regia_platform::VfsReader>> {
                self.0.open("top.webm")
            }
            fn exists(&self, _relative: &str) -> bool {
                true
            }
            fn list_dir(&self, relative: &str) -> std::io::Result<Vec<regia_platform::VfsDirEntry>> {
                self.0.list_dir(relative)
            }
            fn describe(&self) -> String {
                self.0.describe()
            }
        }

        /// Every file under `dir`, as sorted `/`-separated paths relative to `root`.
        fn files_under(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files_under(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).expect("under the root");
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            out.sort();
        }

        let (root, _files) = apk_assets();
        // The destination sits one level inside the sandbox, so an escape lands in the sandbox.
        let sandbox = regia_test_utils::unique_temp_dir("extracted_media_hostile");
        let dest = sandbox.join("content");
        let outside = sandbox.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let absolute = format!("{}/abs.webm", outside.to_string_lossy().replace('\\', "/"));
        let clip = std::fs::read(root.join("top.webm")).expect("clip");
        let entry = |path: &str| PackagedMediaEntry {
            path: path.to_string(),
            len: clip.len() as u64,
            hash: packaged_media_stamp(&clip),
        };
        let index = PackagedMediaIndex {
            format_version: 1,
            files: vec![
                entry("movies/intro.webm"),
                entry("../secret.webm"),
                entry("movies/../../secret2.webm"),
                entry(&absolute),
                entry("movies/./dot.webm"),
                entry("movies//empty.webm"),
                entry("movies\\..\\..\\back.webm"),
                entry("C:drive.webm"),
                entry("content/base.fpak"),
                entry(""),
            ],
        };
        let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty()).expect("index ron");
        std::fs::write(root.join(PACKAGED_MEDIA_INDEX), text).expect("write index");
        let mount = AnyPath(DirMount::read_only(&root));

        let (copied, kept) = extract_packaged_media_indexed(&mount, &dest).expect("extract");
        assert_eq!(
            (copied, kept),
            (1, 0),
            "only the plain relative media path extracts"
        );
        let mut found = Vec::new();
        files_under(&sandbox, &sandbox, &mut found);
        assert_eq!(
            found,
            ["content/movies/intro.webm", "content/movies/intro.webm.stamp"],
            "nothing else is written, inside the destination or outside it"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn without_the_index_the_walk_finds_only_top_level_media_on_an_ndk_mount() {
        /// A mount that lists files only, like the real NDK asset directory.
        struct NdkLike(DirMount);
        impl VfsMount for NdkLike {
            fn read(&self, relative: &str) -> std::io::Result<Vec<u8>> {
                self.0.read(relative)
            }
            fn open(&self, relative: &str) -> std::io::Result<Box<dyn regia_platform::VfsReader>> {
                self.0.open(relative)
            }
            fn exists(&self, relative: &str) -> bool {
                self.0.exists(relative)
            }
            fn list_dir(&self, relative: &str) -> std::io::Result<Vec<regia_platform::VfsDirEntry>> {
                Ok(self
                    .0
                    .list_dir(relative)?
                    .into_iter()
                    .filter(|e| !e.is_dir)
                    .map(|mut e| {
                        e.is_dir = false;
                        e
                    })
                    .collect())
            }
            fn describe(&self) -> String {
                self.0.describe()
            }
        }

        let (root, _files) = apk_assets();
        let mount = NdkLike(DirMount::read_only(&root));
        let dest = regia_test_utils::unique_temp_dir("extracted_media_walk_ndk");
        // A package built before the index existed: the walk sees only the top level.
        let (copied, kept) = extract_packaged_media(&mount, "", &dest).expect("walk");
        assert_eq!((copied, kept), (1, 0), "only the top-level clip is found");
        assert!(dest.join("top.webm").is_file());
        assert!(!dest.join("movies").exists(), "nested media is unreachable");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }
}

#[cfg(test)]
mod log_config_tests {
    use super::log_config;

    /// `logs.keep_files` of `server.toml` reaches the log sink; clients keep the default.
    #[test]
    fn keep_files_reaches_the_log_config() {
        assert_eq!(log_config(None, Some(2)).max_files, 2);
        assert_eq!(log_config(None, Some(0)).max_files, 1, "never zero");
        assert_eq!(log_config(None, None).max_files, 10);
    }
}

#[cfg(all(test, feature = "client"))]
mod audio_env_tests {
    use super::audio_settings_for_env;
    use regia_audio::AudioSettings;

    #[test]
    fn off_forces_headless_and_keeps_the_other_fields() {
        let existing = AudioSettings {
            headless: false,
            sample_rate: 22_050,
            voice_limit: Some(8),
            ..AudioSettings::default()
        };
        let forced = audio_settings_for_env(Some(existing.clone()), Some("off")).expect("settings");
        assert!(forced.headless);
        assert_eq!(forced.sample_rate, 22_050);
        assert_eq!(forced.voice_limit, Some(8));
        assert_eq!(forced.buses, existing.buses);

        let upper = audio_settings_for_env(Some(existing.clone()), Some("OFF")).expect("settings");
        assert!(upper.headless);

        let kept = audio_settings_for_env(Some(existing), Some("on")).expect("settings");
        assert!(!kept.headless);
        assert_eq!(kept.sample_rate, 22_050);

        assert!(audio_settings_for_env(None, None).is_none());
        let inserted = audio_settings_for_env(None, Some("off")).expect("default headless");
        assert!(inserted.headless);
        assert_eq!(inserted.sample_rate, AudioSettings::default().sample_rate);
    }
}

#[cfg(all(test, feature = "client"))]
mod audio_config_tests {
    use super::audio_bus_layout_from;

    /// A single-`Master` layout in `config/audio.ron` text form (a valid tree).
    const MASTER_ONLY: &str = "(buses: [(name: \"Master\", parent: None, volume_db: 0.0, mute: false, effects: [], sends: [], bypass_medium: false)], send_buses: [])";

    fn project_with(config: Option<&str>) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("audio-config");
        if let Some(text) = config {
            std::fs::create_dir_all(root.join("config")).expect("config dir");
            regia_test_utils::write_atomic(&root.join("config/audio.ron"), text.as_bytes())
                .expect("write config");
        }
        root
    }

    /// Spec 16 § 1: `config/audio.ron` is the bus tree the runtime builds.
    #[test]
    fn spec16_section1_config_audio_ron_loads_the_bus_tree() {
        let root = project_with(Some(MASTER_ONLY));
        let loaded = audio_bus_layout_from(&root).expect("the file loads");
        assert_eq!(loaded.buses.len(), 1);
        assert_eq!(loaded.buses[0].name, "Master");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing file, unparseable text or a failing layout keeps the current tree (`None`).
    #[test]
    fn a_missing_or_invalid_config_keeps_the_current_tree() {
        let empty = project_with(None);
        assert!(audio_bus_layout_from(&empty).is_none(), "no file");
        let _ = std::fs::remove_dir_all(&empty);

        let not_ron = project_with(Some("this is not a bus layout"));
        assert!(audio_bus_layout_from(&not_ron).is_none(), "not RON");
        let _ = std::fs::remove_dir_all(&not_ron);

        // Parses but fails validation: `Child` references `Master` before it is defined.
        let forward = "(buses: [(name: \"Child\", parent: Some(\"Master\"), volume_db: 0.0, mute: false, effects: [], sends: [], bypass_medium: false), (name: \"Master\", parent: None, volume_db: 0.0, mute: false, effects: [], sends: [], bypass_medium: false)], send_buses: [])";
        let invalid = project_with(Some(forward));
        assert!(
            audio_bus_layout_from(&invalid).is_none(),
            "a forward parent is rejected"
        );
        let _ = std::fs::remove_dir_all(&invalid);
    }
}

// The Rust examples of the game-modes user-guide page run as doctests (GR-8.2):
// `cargo test -p regia --features net --doc doctests_in_game_modes_page_compile`.
#[cfg(all(doctest, feature = "net"))]
#[doc = include_str!("../../../book/user-guide/game-modes.md")]
mod doctests_in_game_modes_page_compile {}
