// SPDX-License-Identifier: MIT-0
//! The product viewer app (`docs/specs/42-product-viewer.md` § 9): one configurable piece, tap to
//! switch options, drag to turn the view, on the platform layer with a small renderer of its own.
//! A desktop window for development; the Android cdylib in `android/` is the product.
//!
//! The player itself is the web player's core (`regia_web::Player`), so both behave the same. This crate is
//! the host around it: the app holds still when nothing changes (no draw, one wake-up a second, the GPU
//! idle; [`stats::wakes_per_second`]), the surface follows the OS's lifecycle while the device and the open
//! product survive ([`surface`]), an option panel sits over the piece ([`panel`]), and packages are checked
//! and kept on the device, so the app works offline ([`store`]).

pub mod app;
pub mod fetch;
pub mod panel;
pub mod pipeline_cache;
pub mod stats;
pub mod store;
pub mod surface;
pub mod view;
pub mod viewer_api;

use regia_app::prelude::*;
use regia_platform::PlatformPlugin;

#[cfg(target_os = "android")]
pub mod android;

/// The Android activity handle.
#[cfg(target_os = "android")]
pub use regia_platform::AndroidApp;

fn logging() -> Option<regia_core::log::LogGuard> {
    let config = regia_core::log::LogConfig {
        file_dir: None,
        ..Default::default()
    };
    match regia_core::log::init(config) {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!("product viewer: logging setup failed: {err}");
            None
        }
    }
}

fn build_app(app: &mut App, config: app::ViewerConfig) {
    regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
    app.add_plugins(PlatformPlugin);
    app.add_plugins(app::ProductViewerPlugin { config });
}

/// The folders packages are read from on desktop: `--packages DIR`, or `packages` under the app's
/// data folder.
#[cfg(not(target_os = "android"))]
fn desktop_config(args: &[String]) -> app::ViewerConfig {
    let mut config = app::ViewerConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--packages" => config
                .package_roots
                .extend(iter.next().map(std::path::PathBuf::from)),
            "--fetch" => config.fetch_url = iter.next().cloned(),
            "--open" => config.open_package = iter.next().cloned(),
            "--backend" => match iter.next().and_then(|name| surface::BackendChoice::parse(name)) {
                Some(choice) => config.backend = choice,
                None => eprintln!("product viewer: --backend takes auto, vulkan or gl"),
            },
            "--samples" => config.samples = iter.next().and_then(|n| app::parse_samples(n)),
            other => eprintln!("product viewer: ignoring argument {other}"),
        }
    }
    if config.package_roots.is_empty()
        && let Some(user) = regia_platform::paths::user_dir()
    {
        config.package_roots.push(user.join("packages"));
    }
    config.pipeline_cache_dir = regia_platform::paths::user_dir().map(|user| user.join("pipeline-cache"));
    config.push_hint = config
        .package_roots
        .first()
        .map(|root| format!("Copy a package folder to {}", root.display()))
        .unwrap_or_default();
    config
}

/// Runs the desktop window until it closes.
#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> std::process::ExitCode {
    let _log = logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut app = App::new();
    build_app(&mut app, desktop_config(&args));
    #[cfg(feature = "agent")]
    agent::install(&mut app);
    match app.run() {
        AppExit::Success => std::process::ExitCode::SUCCESS,
        AppExit::Error(code) => std::process::ExitCode::from(code.get()),
    }
}

/// Runs the Android app. Never returns: the process ends with the app (ADR-013).
#[cfg(target_os = "android")]
pub fn run_android(android_app: AndroidApp) {
    let _log = logging();
    let config = android::config(&android_app);
    let mut app = App::new();
    app.insert_resource(regia_platform::AndroidAppHandle(android_app));
    build_app(&mut app, config);
    #[cfg(feature = "agent")]
    agent::install(&mut app);
    let exit = app.run();
    tracing::info!(target: "product_viewer", "exited ({exit:?})");
    regia_platform::exit_process(match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    })
}

#[cfg(feature = "agent")]
pub mod agent;
