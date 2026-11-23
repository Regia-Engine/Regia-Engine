// SPDX-License-Identifier: MPL-2.0
//! Audio file live reload (`docs/specs/12-assets.md` § 8): re-saving a `.wav` registered with
//! [`SoundAssets::add_file`] replaces the sound in place within one second (its handle stays
//! valid; the next play uses the new sound). A file that no longer decodes keeps the previous
//! sound.

use std::path::Path;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, SourceKind};
use regia_audio::{SoundAssets, reload_sound_files};

/// A mono 16-bit PCM WAV of `seconds` of a quiet square wave at 22 050 Hz.
fn wav(seconds: f32) -> Vec<u8> {
    let rate = 22_050u32;
    let frames = (rate as f32 * seconds) as u32;
    let data_len = frames * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let v: i16 = if (i / 50) % 2 == 0 { 2_000 } else { -2_000 };
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn write_atomically(path: &Path, bytes: &[u8]) {
    let tmp = path.with_file_name(".regia-tmp-hit");
    std::fs::write(&tmp, bytes).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn re_saving_a_loaded_sound_reloads_it_within_one_second() {
    let root = std::env::temp_dir().join(format!("regia_audio_hot_{}", std::process::id()));
    let dir = root.join("assets").join("sfx");
    std::fs::create_dir_all(&dir).expect("dirs");
    let hit = dir.join("hit.wav");
    std::fs::write(&hit, wav(0.1)).expect("wav");

    let (tx, inbox) = AssetReloadInbox::channel();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(inbox);
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    app.init_resource::<SoundAssets>()
        .add_systems(Update, reload_sound_files);
    let _watch = regia_asset::hot_reload::spawn_source_forwarder(
        std::slice::from_ref(&dir),
        Duration::from_millis(80),
        |kind| kind == SourceKind::Audio,
        tx,
    )
    .expect("watch");
    let handle = app
        .world_mut()
        .resource_mut::<SoundAssets>()
        .add_file("hit", &hit)
        .expect("decode");
    let before = app
        .world()
        .resource::<SoundAssets>()
        .get(&handle)
        .expect("sound")
        .duration_secs();
    assert!((before - 0.1).abs() < 0.01, "{before}");
    app.update();

    let start = Instant::now();
    write_atomically(&hit, &wav(0.3));
    let duration = |app: &App| {
        app.world()
            .resource::<SoundAssets>()
            .get(&handle)
            .map_or(0.0, |s| s.duration_secs())
    };
    while (duration(&app) - before).abs() < 0.01 && start.elapsed() < Duration::from_secs(3) {
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        (duration(&app) - 0.3).abs() < 0.01,
        "new sound live: {}",
        duration(&app)
    );
    let sounds = app.world().resource::<SoundAssets>();
    assert_eq!(sounds.revision("hit"), 1);
    assert_eq!(
        sounds.handle("hit"),
        Some(handle.clone()),
        "the handle stays valid"
    );

    // Garbage: reported, the previous sound stays.
    let errors = app.world().resource::<AssetWatchStatus>().errors;
    write_atomically(&hit, b"RIFF not a wave");
    let start = Instant::now();
    while app.world().resource::<AssetWatchStatus>().errors == errors
        && start.elapsed() < Duration::from_secs(3)
    {
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.world().resource::<AssetWatchStatus>().errors > errors,
        "the bad WAV is reported"
    );
    assert!((duration(&app) - 0.3).abs() < 0.01, "previous sound kept");

    let _ = std::fs::remove_dir_all(&root);
}
