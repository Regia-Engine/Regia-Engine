//! Editor-host link test (no window). Links the engine through the engine dylib and the editor's
//! egui stack statically, runs one headless egui frame, loads the newest game library copy if one
//! exists, and exits. What it measures is in README.md ("editor host").

use spike_regia::bevy_ecs::reflect::AppTypeRegistry;
use spike_regia::regia_app::App;
use spike_regia::hot::{self, GameRegistrar};
use spike_regia::loader;
use spike_regia::prelude::*;

fn main() {
    if std::env::var_os("SPIKE_NEVER").is_some() {
        spike_editor::never_called();
    }
    println!(
        "editor_host: egui frame -> {} shapes; engine ABI {:016x}",
        spike_editor::egui_frame(),
        hot::ABI_HASH
    );
    let Some(lib) = std::env::args().nth(1) else {
        return;
    };
    let mut app = App::new();
    app.init_resource::<AppTypeRegistry>();
    app.init_resource::<PersistentStore>();
    match loader::load_game(std::path::Path::new(&lib)) {
        Ok(plugin) => {
            let mut reg = GameRegistrar::new(&mut app, 1);
            plugin.register(&mut reg);
            let owned = reg.finish();
            plugin.on_load(app.world_mut());
            app.update();
            app.update();
            plugin.on_unload(app.world_mut());
            println!(
                "editor_host: loaded {} ({} owned types), ran 2 frames",
                plugin.name(),
                owned.types.len()
            );
        }
        Err(e) => {
            eprintln!("editor_host: {e}");
            std::process::exit(2);
        }
    }
}
