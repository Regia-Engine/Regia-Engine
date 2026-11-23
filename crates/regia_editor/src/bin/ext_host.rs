// SPDX-License-Identifier: MPL-2.0
//! Scripted extension host for the dynamic extension test (`specs/57` § 8 tests 11 and 12).
//!
//! The process must be linked with `-C prefer-dynamic` and find the toolchain's dynamic `libstd`
//! (see `tests/dynamic_extension.rs`, which builds it). Two commands:
//!
//! - `env --target-dir T` prints the `REGIA_ABI_HASH` and `REGIA_ENGINE_FINGERPRINT` a library
//!   must be built with to load here.
//! - `run --project P --target-dir T --first A --second B` loads the extension at
//!   `P/extensions/ext_fixture`, flips each stamp field of a copy of `A`, truncates one, then
//!   reloads `B` twice and once more with a failing build. After each step one frame runs and
//!   reports which build's system ran. Each result is a line `obs <key> <value>`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regia_agent::schema::METHODS;
use regia_editor::engine::app::prelude::*;
use regia_editor::extension::host::{self, library_path};
use regia_editor::extension::load::{self, check_library_file, entry_calls, expected_stamp};
use regia_editor::extension::{EditorExtensionPlugin, ExtensionLoadError, load_extension};
use regia_framework::extension::discover::MANIFEST_FILE;
use regia_framework::extension::stamp::STAMP_FIELDS;
use regia_framework::extension::{ExtensionRegistry, LevelFeatures};
use serde_json::{Value, json};

const ID: &str = "ext_fixture";

struct Args {
    project: PathBuf,
    target_dir: PathBuf,
    first: PathBuf,
    second: PathBuf,
}

fn obs(key: &str, value: impl std::fmt::Display) {
    println!("obs {key} {value}");
}

fn code(result: Result<load::LoadedExtension, ExtensionLoadError>) -> String {
    match result {
        Ok(_) => "loaded".to_owned(),
        Err(ExtensionLoadError::AbiMismatch { field }) => format!("abi_mismatch:{field}"),
        Err(error) => error.code().to_owned(),
    }
}

/// Spec § 8 test 11, on copies under `scratch`; nothing here may reach an entry symbol.
fn stamp_refusals(
    library: &Path,
    expected: &regia_framework::extension::stamp::ExtensionStamp,
    scratch: &Path,
) -> Result<(), String> {
    let bytes = std::fs::read(library).map_err(|e| e.to_string())?;
    let offset = stamp_symbol_offset(&bytes)?;
    obs("file_check", check_library_file(library, expected).is_ok());
    obs(
        "magic_records",
        bytes.windows(8).filter(|window| *window == b"REGIAEXT").count(),
    );
    let calls = entry_calls();
    for (field, at, _) in STAMP_FIELDS {
        let mut copy = bytes.clone();
        copy[offset + at] ^= 0x41;
        let path = scratch.join(format!("flip-{field}.so"));
        std::fs::write(&path, copy).map_err(|e| e.to_string())?;
        // Each case has its own directory: the copy is what proves the loader was reached.
        let hot = scratch.join(format!("refused-{field}"));
        obs(
            &format!("flip_{field}"),
            code(load_extension(&path, expected, &hot, ID, 900)),
        );
        obs(&format!("flip_{field}_copied"), hot.is_dir());
    }
    // Every magic gone: the exported record has none either.
    let starts: Vec<usize> = bytes
        .windows(8)
        .enumerate()
        .filter(|(_, window)| *window == b"REGIAEXT")
        .map(|(at, _)| at)
        .collect();
    let mut no_magic = bytes.clone();
    for at in &starts {
        no_magic[*at] ^= 0x41;
    }
    // A Rust dylib may carry the record elsewhere too (crate metadata). Only the exported symbol
    // counts (ADR-2844): damaging any other copy changes nothing, and that is the point.
    let decoys: Vec<usize> = starts.iter().copied().filter(|at| *at != offset).collect();
    obs("decoy_records", decoys.len());
    let mut decoyed = bytes.clone();
    for at in &decoys {
        decoyed[at + STAMP_FIELDS[4].1] ^= 0x41;
        decoyed[*at] ^= 0x41;
    }
    let path = scratch.join("decoys-damaged.so");
    std::fs::write(&path, decoyed).map_err(|e| e.to_string())?;
    obs("decoys_ignored", check_library_file(&path, expected).is_ok());
    let path = scratch.join("no-magic.so");
    std::fs::write(&path, no_magic).map_err(|e| e.to_string())?;
    let hot = scratch.join("refused-no-magic");
    obs("no_magic", code(load_extension(&path, expected, &hot, ID, 902)));
    obs("no_magic_copied", hot.is_dir());
    let short = scratch.join("short.so");
    std::fs::write(&short, &bytes[..offset + 16]).map_err(|e| e.to_string())?;
    let hot = scratch.join("refused-short");
    obs("truncated", code(load_extension(&short, expected, &hot, ID, 901)));
    obs("truncated_copied", hot.is_dir());
    obs("entry_calls_after_refusals", entry_calls() - calls);
    Ok(())
}

/// File offset of the exported `REGIA_EXTENSION_STAMP` record, found with the host's own reader.
fn stamp_symbol_offset(file: &[u8]) -> Result<usize, String> {
    use regia_framework::extension::objfile::ObjFile;
    use regia_framework::extension::stamp::{STAMP_DATA_SYMBOL, STAMP_SIZE};
    let object = ObjFile::parse(file).map_err(|e| e.to_string())?;
    let record = object
        .exported_data(STAMP_DATA_SYMBOL, STAMP_SIZE)
        .map_err(|e| e.to_string())?;
    Ok(record.as_ptr() as usize - file.as_ptr() as usize)
}

fn marker(app: &mut App) -> Result<Value, String> {
    let handler = {
        let registry = app.world().resource::<ExtensionRegistry>();
        let (_, method) = registry
            .agent_method("ext_fixture.marker")
            .ok_or("ext_fixture.marker is not registered")?;
        std::sync::Arc::clone(&method.handler)
    };
    handler(app.world_mut(), json!({})).map_err(|fault| fault.code)
}

/// Runs one frame and reports how many times each build's system ran in it.
fn tick_line(app: &mut App, key: &str) {
    let count = |app: &mut App, marker: &str| {
        let mut query = app.world_mut().query::<&LevelFeatures>();
        query
            .iter(app.world())
            .filter(|f| f.features.iter().any(|name| name == marker))
            .count()
    };
    let before = (count(app, "tick:one"), count(app, "tick:two"));
    app.update();
    let after = (count(app, "tick:one"), count(app, "tick:two"));
    obs(
        &format!("{key}_ticks"),
        format!("one={} two={}", after.0 - before.0, after.1 - before.1),
    );
}

fn reload_line(app: &mut App, key: &str) -> Result<(), String> {
    match host::reload(app.world_mut(), &json!({ "id": ID })) {
        Ok(answer) => {
            obs(&format!("{key}_generation"), &answer["generation"]);
            obs(&format!("{key}_removed"), &answer["removed"]["agent_method"]);
            obs(&format!("{key}_added"), &answer["added"]["agent_method"]);
        }
        Err(error) => {
            let data = error.data.unwrap_or_default();
            obs(&format!("{key}_error"), data["code"].as_str().unwrap_or("none"));
        }
    }
    let answer = marker(app)?;
    obs(
        &format!("{key}_marker"),
        answer["marker"].as_str().unwrap_or("none"),
    );
    obs(&format!("{key}_count"), &answer["count"]);
    tick_line(app, key);
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    let dir = args.project.join("extensions").join(ID);
    let manifest = std::fs::read(dir.join(MANIFEST_FILE)).map_err(|e| e.to_string())?;
    let expected = expected_stamp(&manifest, &args.target_dir);
    stamp_refusals(&args.first, &expected, &args.project)?;

    let found = regia_framework::extension::discover::read_extension_dir(&dir).map_err(|d| d.message)?;
    let code = found.manifest.code.first().ok_or("no code entry")?;
    let installed = library_path(&dir, code).ok_or("the fixture library is not installed")?;
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(EditorExtensionPlugin {
        project: Some(args.project.clone()),
        target_dir: Some(args.target_dir.clone()),
        engine_dir: None,
        linked: Vec::new(),
    });
    app.update();
    {
        let registry = app.world().resource::<ExtensionRegistry>();
        obs("start_state", registry.state(ID).map_or("none", |s| s.name()));
        obs("start_kind", &registry.list_json(None)["extensions"][0]["kind"]);
        obs("start_codes", registry.codes_for(ID).join(","));
    }
    let first = marker(&mut app)?;
    obs("start_marker", first["marker"].as_str().unwrap_or("none"));
    tick_line(&mut app, "start");

    std::fs::copy(&args.second, &installed).map_err(|e| e.to_string())?;
    reload_line(&mut app, "reload1")?;
    reload_line(&mut app, "reload2")?;

    // A failing build restores the old registrations; the old code keeps answering.
    std::fs::write(dir.join("fail-build"), b"").map_err(|e| e.to_string())?;
    reload_line(&mut app, "failed")?;
    std::fs::remove_file(dir.join("fail-build")).map_err(|e| e.to_string())?;
    reload_line(&mut app, "recovered")?;

    // A library built for something else is refused and changes nothing.
    let mut bytes = std::fs::read(&installed).map_err(|e| e.to_string())?;
    let offset = stamp_symbol_offset(&bytes)?;
    bytes[offset + STAMP_FIELDS[4].1] ^= 0x41;
    std::fs::write(&installed, bytes).map_err(|e| e.to_string())?;
    reload_line(&mut app, "refused")?;
    obs(
        "schema_has_reload",
        METHODS.iter().any(|m| m.name == "plugin.reload"),
    );
    Ok(())
}

fn parse() -> Result<(String, Args), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("missing command")?;
    let (mut project, mut target_dir, mut first, mut second) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = PathBuf::from(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        match flag.as_str() {
            "--project" => project = Some(value),
            "--target-dir" => target_dir = Some(value),
            "--first" => first = Some(value),
            "--second" => second = Some(value),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok((
        command,
        Args {
            project: project.unwrap_or_default(),
            target_dir: target_dir.ok_or("missing --target-dir")?,
            first: first.unwrap_or_default(),
            second: second.unwrap_or_default(),
        },
    ))
}

fn main() -> ExitCode {
    let result = parse().and_then(|(command, args)| match command.as_str() {
        "env" => {
            let stamp = expected_stamp(b"", &args.target_dir);
            println!("{} {}", stamp.abi_hash, stamp.fingerprint());
            Ok(())
        }
        "run" => run(&args),
        other => Err(format!("unknown command {other}")),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("regia-ext-host: {error}");
            ExitCode::FAILURE
        }
    }
}
