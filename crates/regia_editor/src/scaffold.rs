// SPDX-License-Identifier: MPL-2.0
//! Blank-project scaffolding for the editor (`docs/specs/19` § 6, `docs/specs/23` § 8).
//!
//! This is a copy of `regia_cli::new_project`. Both crates are the Tools layer, and a
//! same-layer dependency is rejected by `cargo xtask check-layers`, so the editor cannot
//! call the CLI crate (and the CLI cannot call the editor). Keep the two copies in step:
//! placeholders `{{name}}`, `{{id}}`, `{{crate_name}}`, `{{engine}}`, `{{engine_line}}`, the
//! seeded `Cargo.lock` (spec 94 § 3.2) and the file list of
//! `crates/regia_templates/templates/blank/`.

use std::path::{Path, PathBuf};

/// What the project browser or `editor.new_project` asked to create.
#[derive(Debug, Clone)]
pub(crate) struct NewProjectRequest {
    /// Display name (`Regia.toml` `[project] name`). Also the source of the crate name.
    pub name: String,
    /// Template id. Same set as `regia new`: `blank`, `first_person`, `third_person`, `top_down`.
    pub template: String,
    /// Directory the project folder is created in.
    pub parent: PathBuf,
}

/// A project directory the editor just wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatedProject {
    /// The project root (contains `Regia.toml`).
    pub root: PathBuf,
    /// Rust package name of the game crate.
    pub crate_name: String,
    /// Reverse-DNS id written into `Regia.toml`.
    pub id: String,
}

/// Why scaffolding refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NewProjectError {
    /// The template is not one the engine ships (same set as `regia new`).
    #[error(
        "unknown template `{0}` (this engine ships `blank`, `first_person`, `third_person`, and `top_down`)"
    )]
    UnknownTemplate(String),
    /// The name cannot become a Rust package name, or it would break the TOML.
    #[error("name `{0}` is empty, not a valid crate name, or contains quotes")]
    BadName(String),
    /// The destination exists and is not an empty directory.
    #[error("project directory already exists: {0}")]
    Exists(String),
    /// A write failed, or the engine checkout could not be resolved.
    #[error("{0}")]
    Io(String),
}

/// The files of a template, by template id: the set `regia new` ships (`regia_templates`), minus
/// the single-crate `quick` template, which the project browser does not offer.
fn template_files(template: &str) -> Option<&'static [(&'static str, &'static str)]> {
    if template == "quick" {
        return None;
    }
    regia_templates::template_files(template)
}

/// The engine checkout this editor runs from (ADR-10544, the rule of `regia_cli::new_project::engine_checkout`):
/// `REGIA_ENGINE_DIR` when it names a checkout; else the nearest ancestor of the running executable
/// that holds `crates/regia/Cargo.toml`; else, in debug builds only, the checkout this binary was
/// compiled in. A release build carries no build directory (ADR-10354). The samples browser lists
/// the same checkout.
pub(crate) fn engine_root() -> Result<PathBuf, NewProjectError> {
    engine_checkout().ok_or_else(|| {
        NewProjectError::Io(
            "no engine checkout found: this editor is not inside one and REGIA_ENGINE_DIR is not set. \
             Create the project with `regia new <name>` (it pins the engine release) and open it here, \
             or set REGIA_ENGINE_DIR to an engine checkout"
                .to_string(),
        )
    })
}

/// [`engine_root`] as an option (also the engine directory the extension host defaults to).
pub(crate) fn engine_checkout() -> Option<PathBuf> {
    let override_dir = regia_core::brand::env_var("REGIA_ENGINE_DIR")
        .ok()
        .map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    find_engine_checkout(override_dir, exe.as_deref(), compiled_checkout())
}

/// The checkout a debug build was compiled in; `None` in release builds, so the path is not in
/// the binary (ADR-10354, ADR-10544).
fn compiled_checkout() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// The override, the executable's ancestors, then the compiled-in checkout: the first directory
/// with `crates/regia/Cargo.toml`, canonicalized when it can be, without Windows' verbatim `\\?\`
/// prefix (Cargo refuses `path = "//?/C:/…"`, ADR-10616).
fn find_engine_checkout(
    override_dir: Option<PathBuf>,
    exe: Option<&Path>,
    compiled: Option<PathBuf>,
) -> Option<PathBuf> {
    let is_checkout = |dir: &Path| dir.join("crates/regia/Cargo.toml").is_file();
    let from_exe = exe.and_then(|exe| {
        exe.ancestors()
            .skip(1)
            .find(|dir| is_checkout(dir))
            .map(Path::to_path_buf)
    });
    override_dir
        .into_iter()
        .chain(from_exe)
        .chain(compiled)
        .find(|dir| is_checkout(dir))
        .map(|dir| regia_core::project_path::canonicalize_plain(&dir).unwrap_or(dir))
}

/// Turns a display name into a Rust package name (`"My Game"` → `my_game`), by the rule
/// `regia new` and the editor share (`regia_templates::project_crate_name`): Rust and Java
/// reserved words and Windows device names are refused.
pub(crate) fn crate_name_for(name: &str) -> Result<String, NewProjectError> {
    regia_templates::project_crate_name(name).ok_or_else(|| NewProjectError::BadName(name.to_string()))
}

/// Append this machine's home directory as the last path remap (ADR-10370).
fn home_remap_config(config: &str) -> Result<String, NewProjectError> {
    let home = regia_templates::user_home_dir().ok_or_else(|| {
        NewProjectError::Io(
            "HOME (or USERPROFILE) is unset, so the home path remap cannot be written (ADR-10370)".into(),
        )
    })?;
    regia_templates::with_home_path_remap(config, &home).map_err(NewProjectError::Io)
}

fn substitute(text: &str, name: &str, id: &str, crate_name: &str, engine: &str) -> String {
    text.replace("{{crate_name}}", crate_name)
        .replace(
            "{{engine_line}}",
            &regia_core::engine_version::CURRENT.line().to_string(),
        )
        .replace("{{engine}}", engine)
        .replace("{{name}}", name)
        .replace("{{id}}", id)
}

/// Creates the project directory and returns it.
///
/// # Errors
///
/// Unknown template, a name that is not a crate name, a destination that already exists,
/// or an IO failure.
pub(crate) fn create(request: &NewProjectRequest) -> Result<CreatedProject, NewProjectError> {
    let files = template_files(&request.template)
        .ok_or_else(|| NewProjectError::UnknownTemplate(request.template.clone()))?;
    let crate_name = crate_name_for(&request.name)?;
    let engine = engine_root()?;
    let engine_text = regia_core::project_path::strip_verbatim(&engine)
        .to_string_lossy()
        .replace('\\', "/");
    if engine_text.contains('"') {
        return Err(NewProjectError::Io(format!(
            "engine path {} cannot be written into Cargo.toml",
            engine.display()
        )));
    }
    let id = format!("com.regiaengine.{crate_name}");
    let root = request.parent.join(&crate_name);
    if !regia_templates::project_dir_is_free(&root) {
        return Err(NewProjectError::Exists(root.display().to_string()));
    }
    // Every substitution is checked before the target is claimed, and a failed write removes what
    // was written (`regia_templates::write_project_dir`, shared with `regia new`; spec 61 § 2.1).
    let mut rendered = Vec::with_capacity(files.len());
    for (rel, text) in files {
        let mut body = substitute(text, request.name.trim(), &id, &crate_name, &engine_text);
        if body.contains("{{") {
            return Err(NewProjectError::Io(format!(
                "template {rel} still has an unsubstituted placeholder"
            )));
        }
        if *rel == ".cargo/config.toml" {
            body = home_remap_config(&body)?;
        }
        rendered.push((*rel, body));
    }
    regia_templates::write_project_dir(&root, &rendered).map_err(|err| match err {
        regia_templates::ProjectDirError::Exists(path) => NewProjectError::Exists(path.display().to_string()),
        regia_templates::ProjectDirError::Io(text) => NewProjectError::Io(text),
    })?;
    // The engine's tested dependency versions (spec 94 § 3.2): a first build resolves what the
    // engine was tested with. `regia_cli::distribution::seed_lockfile` is the CLI's copy.
    let lock = engine.join("Cargo.lock");
    if lock.is_file() {
        let staged = root.join("Cargo.lock.tmp");
        std::fs::copy(&lock, &staged)
            .and_then(|_| std::fs::rename(&staged, root.join("Cargo.lock")))
            .map_err(|err| NewProjectError::Io(format!("cannot seed {}: {err}", staged.display())))?;
    }
    tracing::info!(target: "regia_editor", "created project {} ({crate_name})", root.display());
    Ok(CreatedProject { root, crate_name, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-10544: a release editor outside any checkout finds none; inside one (or with the
    /// override) it does, and a directory without `crates/regia` is never taken.
    #[test]
    fn the_engine_checkout_is_found_at_run_time() {
        let root = regia_test_utils::unique_temp_dir("scaffold-checkout");
        let engine = root.join("engine");
        std::fs::create_dir_all(engine.join("crates/regia")).expect("engine dir");
        std::fs::write(engine.join("crates/regia/Cargo.toml"), "[package]\n").expect("manifest");
        let exe = engine.join("target/release/regia-editor");
        let lone = root.join("download");
        std::fs::create_dir_all(&lone).expect("download dir");
        let lone_exe = lone.join("regia-editor");
        let found = Some(regia_core::project_path::canonicalize_plain(&engine).expect("canonical"));
        assert_eq!(find_engine_checkout(None, Some(&exe), None), found);
        assert_eq!(find_engine_checkout(None, Some(&lone_exe), None), None);
        assert_eq!(
            find_engine_checkout(Some(lone.clone()), Some(&lone_exe), None),
            None
        );
        assert_eq!(
            find_engine_checkout(Some(engine.clone()), Some(&lone_exe), None),
            found
        );
        assert_eq!(find_engine_checkout(None, Some(&lone_exe), Some(engine)), found);
        assert!(engine_checkout().is_some_and(|dir| dir.join("crates/regia_editor/Cargo.toml").is_file()));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_parent(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-new-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("parent");
        dir
    }

    #[test]
    fn crate_names_are_snake_case_and_reject_junk() {
        assert_eq!(crate_name_for("My Game").expect("name"), "my_game");
        assert_eq!(crate_name_for("hello-world").expect("name"), "hello_world");
        assert!(crate_name_for("").is_err());
        assert!(crate_name_for("123abc").is_err());
        assert!(crate_name_for("fn").is_err());
        assert!(crate_name_for("say \"hi\"").is_err());
    }

    /// Spec 61 § 2.1 `generate` and § 10 test 2: a template that cannot be filled in (here the name
    /// itself carries a placeholder) writes nothing, so a retry with a good name is not refused.
    #[test]
    fn a_failed_generation_leaves_no_partial_project() {
        let parent = temp_parent("partial");
        let result = create(&NewProjectRequest {
            name: "Hello {{oops}}".into(),
            template: "blank".into(),
            parent: parent.clone(),
        });
        assert!(matches!(result, Err(NewProjectError::Io(_))), "{result:?}");
        assert!(!parent.join("hello_oops").exists(), "nothing remains");
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Spec 61 § 2.1 `location`: an existing empty folder is a valid target.
    #[test]
    fn an_empty_target_folder_is_used() {
        let parent = temp_parent("empty_target");
        std::fs::create_dir(parent.join("my_game")).expect("empty folder");
        let created = create(&NewProjectRequest {
            name: "My Game".into(),
            template: "blank".into(),
            parent: parent.clone(),
        })
        .expect("an empty folder is free");
        assert!(created.root.join("Regia.toml").is_file());
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// The project browser refuses the names `regia new` refuses (spec 61 § 1: one create path):
    /// Java keywords, because the crate name is the Android application id's last segment
    /// (ADR-5291), and Windows device names, because it is the project folder (spec 61 § 11).
    #[test]
    fn java_keywords_and_device_names_are_refused_like_the_cli() {
        for name in ["class", "package", "new", "con", "NUL", "com1", "lpt9"] {
            assert!(
                matches!(crate_name_for(name), Err(NewProjectError::BadName(_))),
                "{name:?}"
            );
        }
        assert_eq!(crate_name_for("console").expect("name"), "console");
    }

    #[test]
    fn blank_template_writes_a_project_that_names_itself() {
        let parent = temp_parent("blank");
        let created = create(&NewProjectRequest {
            name: "My Game".into(),
            template: "blank".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "my_game");
        assert_eq!(created.id, "com.regiaengine.my_game");
        assert_eq!(created.root, parent.join("my_game"));
        let manifest = std::fs::read_to_string(created.root.join("Regia.toml")).expect("manifest");
        assert!(manifest.contains("name = \"My Game\""), "{manifest}");
        assert!(manifest.contains("startup_level = \"levels/main.level.ron\""));
        let line = regia_core::engine_version::CURRENT.line();
        assert!(manifest.contains(&format!("engine = \"{line}\"")), "{manifest}");
        assert_eq!(
            std::fs::read(created.root.join("Cargo.lock")).expect("seeded lock"),
            std::fs::read(engine_root().expect("engine").join("Cargo.lock")).expect("engine lock"),
            "the project starts from the engine's tested versions (spec 94 § 3.2)"
        );
        assert!(!created.root.join("Cargo.lock.tmp").exists());
        assert!(created.root.join("assets/levels/main.level.ron").is_file());
        assert!(created.root.join("game/src/lib.rs").is_file());
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml")).expect("game toml");
        assert!(game.contains("name = \"my_game\""), "{game}");
        assert!(game.contains("crates/regia\""), "path dep: {game}");
        assert!(!game.contains("{{"), "unsubstituted: {game}");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        let again = create(&NewProjectRequest {
            name: "My Game".into(),
            template: "blank".into(),
            parent,
        });
        assert!(again.is_err(), "the directory exists");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    #[test]
    fn unknown_templates_are_refused() {
        let err = create(&NewProjectRequest {
            name: "X".into(),
            // `third-person` is an alias since 20adffba6; this name is not a template.
            template: "side-scroller".into(),
            parent: std::env::temp_dir(),
        })
        .expect_err("tier");
        let message = err.to_string();
        assert!(message.contains("blank"), "{message}");
        assert!(message.contains("first_person"), "{message}");
        assert!(message.contains("third_person"), "{message}");
    }

    /// `editor.new_project` accepts every template `regia new` ships (the guides-review bug:
    /// the editor refused `third_person` although the CLI scaffolded it).
    #[test]
    fn every_cli_template_scaffolds_through_the_editor() {
        for template in ["blank", "third_person", "first_person", "top_down"] {
            let parent = temp_parent(template);
            let created = create(&NewProjectRequest {
                name: "Editor Game".into(),
                template: template.into(),
                parent: parent.clone(),
            })
            .unwrap_or_else(|err| panic!("{template}: {err}"));
            assert_eq!(created.crate_name, "editor_game", "{template}");
            assert!(
                created.root.join("game/src/lib.rs").is_file(),
                "{template}: game lib missing"
            );
            let game = std::fs::read_to_string(created.root.join("game/Cargo.toml"))
                .unwrap_or_else(|err| panic!("{template}: {err}"));
            assert!(!game.contains("{{"), "{template}: unsubstituted: {game}");
            let _ = std::fs::remove_dir_all(&created.root);
        }
    }

    #[test]
    fn third_person_template_writes_a_playable_starter() {
        let parent = temp_parent("editor-third");
        let created = create(&NewProjectRequest {
            name: "Hero Quest".into(),
            template: "third_person".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "hero_quest");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        // The boom camera lives in the character module (ADR-2559).
        let character =
            std::fs::read_to_string(created.root.join("game/src/character.rs")).expect("character");
        assert!(character.contains("CameraMode::ThirdPerson"), "{character}");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    /// ADR-10370: the editor's copy of `regia new` appends this machine's home remap last.
    #[test]
    fn created_project_appends_the_home_remap_last() {
        let parent = temp_parent("editor-remap");
        let created = create(&NewProjectRequest {
            name: "Remap Check".into(),
            template: "blank".into(),
            parent: parent.clone(),
        })
        .expect("create");
        let config = std::fs::read_to_string(created.root.join(".cargo/config.toml")).expect("config");
        let home = regia_templates::user_home_dir().expect("HOME");
        let flag = regia_templates::home_remap_flag(&home).expect("flag");
        for array in config.split("rustflags = [").skip(1) {
            let body = array.split(']').next().unwrap_or("");
            let last = body
                .lines()
                .rev()
                .find(|line| line.contains("remap-path-prefix"))
                .unwrap_or("");
            assert!(last.contains(&flag), "{last}");
        }
        let _ = std::fs::remove_dir_all(&parent);
    }
}
