// SPDX-License-Identifier: MPL-2.0
//! `regia new <name> --template blank|first_person|third_person|top_down|quick|net-game` (`docs/specs/21` § 1 and § 3,
//! `docs/specs/19` § 6).
//!
//! Templates are embedded from `crates/regia_templates/templates/<id>/` (placeholders `{{name}}`, `{{id}}`,
//! `{{crate_name}}`, `{{engine}}`, `{{engine_line}}`). `{{engine}}` is this checkout: a generated
//! project path-depends on it so `cargo run -p <name>_launcher` builds against the same
//! engine; `{{engine_line}}` is its release line (spec 94 § 4.2). First Person is the stripped-down `samples/arena` (M20). The Third Person
//! template is the M20 sample stripped to a playable starter. The Quick template is one crate with one
//! `src/main.rs` on `regia::quick` (no editor, launcher or Android crates).
//!
//! `regia_editor` keeps a copy of this module (`scaffold`). The two crates are both
//! Tools, so they cannot share it through a dependency.

use std::path::{Path, PathBuf};

/// What `regia new` was asked to create.
#[derive(Debug, Clone)]
pub struct NewProjectRequest {
    /// Display name (`Regia.toml` `[project] name`). Also the source of the crate name.
    pub name: String,
    /// Template id. M11 accepts `blank`; M20 adds `first_person` and `third_person`; spec 91 adds
    /// `top_down`; `quick` is the single-file quick app; `net-game` (also `net_game`) is the dedicated
    /// server and client of ADR-2039.
    pub template: String,
    /// Directory the project folder is created in.
    pub parent: PathBuf,
}

/// A project directory `regia new` just wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProject {
    /// The project root (contains `Regia.toml`).
    pub root: PathBuf,
    /// Rust package name of the game crate.
    pub crate_name: String,
    /// Reverse-DNS id written into `Regia.toml`.
    pub id: String,
}

/// Why `regia new` refused.
#[derive(Debug, thiserror::Error)]
pub enum NewProjectError {
    /// The template is not one this milestone ships.
    #[error(
        "unknown template `{0}` (available: `blank`, `first_person`, `third_person`, `top_down`, `quick`, and `net-game`)"
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

/// The files of a template, by template id (the tables live in `regia_templates`).
fn template_files(template: &str) -> Option<&'static [(&'static str, &'static str)]> {
    regia_templates::template_files(template)
}

/// The engine checkout this binary runs from, found at run time (ADR-10544): `REGIA_ENGINE_DIR`
/// when it names a checkout; else the nearest ancestor of the running executable that holds
/// `crates/regia/Cargo.toml` (a `target/<profile>/` build inside a checkout); else, in debug builds
/// only, the checkout this binary was compiled in. A release build carries no build directory
/// (ADR-10354), so a downloaded `regia` outside a checkout finds none.
#[must_use]
pub fn engine_checkout() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    find_engine_checkout(
        regia_core::brand::env_var("REGIA_ENGINE_DIR")
            .ok()
            .map(PathBuf::from),
        exe.as_deref(),
        compiled_checkout(),
    )
}

/// The checkout a debug build was compiled in (`crates/regia_cli` → workspace root); release
/// builds return `None` so the path is not in the binary (ADR-10354, ADR-10544).
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

/// [`engine_checkout`]'s order over explicit inputs: the override, the executable's ancestors,
/// then the compiled-in checkout; the first directory with `crates/regia/Cargo.toml` wins.
#[must_use]
pub fn find_engine_checkout(
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
        .map(|dir| dir.canonicalize().unwrap_or(dir))
}

/// The engine checkout `create` writes into the templates.
fn engine_root() -> Result<PathBuf, NewProjectError> {
    engine_checkout().ok_or_else(|| {
        NewProjectError::Io(
            "no engine checkout found (this `regia` is not inside one and REGIA_ENGINE_DIR is not set); \
             pass `--engine-source git:<url>#<tag>` or `--engine-source path:<checkout>`"
                .to_string(),
        )
    })
}

/// Turns a display name into a Rust package name (`"My Game"` → `my_game`), by the rule
/// `regia new` and the editor share (`regia_templates::project_crate_name`): Rust and Java
/// reserved words and Windows device names are refused.
pub fn crate_name_for(name: &str) -> Result<String, NewProjectError> {
    regia_templates::project_crate_name(name).ok_or_else(|| NewProjectError::BadName(name.to_string()))
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
pub fn create(request: &NewProjectRequest) -> Result<CreatedProject, NewProjectError> {
    // Check the request before the engine, so a bad name or template is reported as such.
    template_files(&request.template)
        .ok_or_else(|| NewProjectError::UnknownTemplate(request.template.clone()))?;
    crate_name_for(&request.name)?;
    let engine = engine_root()?;
    create_with_engine(request, &engine.to_string_lossy())
}

/// [`create`] with the `{{engine}}` text given instead of found: the engine checkout's path, or,
/// when the caller re-pins the project right after (`regia new --engine-source` without a local
/// checkout, ADR-10544), the source's own text, which the re-pin replaces in every manifest.
///
/// # Errors
///
/// As [`create`], plus an engine text with a quote.
pub fn create_with_engine(
    request: &NewProjectRequest,
    engine: &str,
) -> Result<CreatedProject, NewProjectError> {
    let files = template_files(&request.template)
        .ok_or_else(|| NewProjectError::UnknownTemplate(request.template.clone()))?;
    let crate_name = crate_name_for(&request.name)?;
    let engine_text = engine.replace('\\', "/");
    if engine_text.contains('"') {
        return Err(NewProjectError::Io(format!(
            "engine path {engine} cannot be written into Cargo.toml"
        )));
    }
    let id = format!("com.regiaengine.{crate_name}");
    let root = request.parent.join(&crate_name);
    if !regia_templates::project_dir_is_free(&root) {
        return Err(NewProjectError::Exists(root.display().to_string()));
    }
    // Validate every substitution before claiming a destination or writing any files.
    let mut rendered = Vec::with_capacity(files.len());
    for (rel, text) in files {
        let body = substitute(text, request.name.trim(), &id, &crate_name, &engine_text);
        if body.contains("{{") {
            return Err(NewProjectError::Io(format!(
                "template {rel} still has an unsubstituted placeholder"
            )));
        }
        rendered.push((*rel, body));
    }
    write_project_files(&root, &rendered)?;
    tracing::info!(target: "regia_cli", "created project {} ({crate_name})", root.display());
    Ok(CreatedProject { root, crate_name, id })
}

/// Claims the target (new, or an empty directory) and writes the files; on a failure nothing this
/// call wrote remains (`regia_templates::write_project_dir`, shared with the editor).
fn write_project_files(root: &Path, files: &[(&str, String)]) -> Result<(), NewProjectError> {
    regia_templates::write_project_dir(root, files).map_err(|err| match err {
        regia_templates::ProjectDirError::Exists(path) => NewProjectError::Exists(path.display().to_string()),
        regia_templates::ProjectDirError::Io(text) => NewProjectError::Io(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_parent(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-new-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("parent");
        dir
    }

    /// ADR-10544: the checkout is found at run time (override, then the executable's ancestors,
    /// then the compiled-in one), and a directory without `crates/regia` is never taken.
    #[test]
    fn the_engine_checkout_is_found_at_run_time() {
        let root = temp_parent("checkout");
        let engine = root.join("engine");
        std::fs::create_dir_all(engine.join("crates/regia")).expect("engine dir");
        std::fs::write(engine.join("crates/regia/Cargo.toml"), "[package]\n").expect("manifest");
        let exe_dir = engine.join("target/release");
        std::fs::create_dir_all(&exe_dir).expect("exe dir");
        let exe = exe_dir.join("regia");
        let lone = root.join("download");
        std::fs::create_dir_all(&lone).expect("download dir");
        let lone_exe = lone.join("regia");
        let found = |dir: PathBuf| Some(dir.canonicalize().expect("canonical"));
        // A release build inside a checkout finds it from the executable.
        assert_eq!(
            find_engine_checkout(None, Some(&exe), None),
            found(engine.clone())
        );
        // A downloaded binary outside any checkout, with no compiled-in path, finds none.
        assert_eq!(find_engine_checkout(None, Some(&lone_exe), None), None);
        assert_eq!(find_engine_checkout(None, None, None), None);
        // The override wins, but only when it is a checkout.
        assert_eq!(
            find_engine_checkout(Some(engine.clone()), Some(&lone_exe), None),
            found(engine.clone())
        );
        assert_eq!(
            find_engine_checkout(Some(lone.clone()), Some(&lone_exe), None),
            None
        );
        // The compiled-in checkout is the last resort.
        assert_eq!(
            find_engine_checkout(Some(lone.clone()), Some(&lone_exe), Some(engine.clone())),
            found(engine)
        );
        // This debug test binary finds the workspace it was built in.
        assert!(engine_checkout().is_some_and(|dir| dir.join("crates/regia_cli/Cargo.toml").is_file()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-10544: with the engine text given, no checkout is needed; the text lands in every
    /// engine dependency for the re-pin to replace.
    #[test]
    fn create_with_engine_writes_the_given_engine_text() {
        let parent = temp_parent("with-engine");
        let created = create_with_engine(
            &NewProjectRequest {
                name: "Pinned".into(),
                template: "blank".into(),
                parent: parent.clone(),
            },
            "git:https://example.invalid/engine#v1",
        )
        .expect("create");
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml")).expect("game manifest");
        assert!(
            game.contains("path = \"git:https://example.invalid/engine#v1/crates/regia\""),
            "{game}"
        );
        assert!(matches!(
            create_with_engine(
                &NewProjectRequest {
                    name: "Quoted".into(),
                    template: "blank".into(),
                    parent: parent.clone(),
                },
                "a\"b",
            ),
            Err(NewProjectError::Io(_))
        ));
        let _ = std::fs::remove_dir_all(&parent);
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

    /// Spec 61 § 10 test 3: names with spaces, uppercase, leading digits, unicode and reserved words
    /// normalize or fail, and the result is a crate name and a folder name on every OS.
    #[test]
    fn names_normalize_or_fail_per_the_table() {
        let table: &[(&str, Option<&str>)] = &[
            ("My Game", Some("my_game")),
            ("  Spaced   Out  ", Some("spaced_out")),
            ("UPPER", Some("upper")),
            ("hello-world", Some("hello_world")),
            ("Café Racer", Some("caf_racer")),
            ("console", Some("console")),
            ("com10", Some("com10")),
            ("9lives", None),
            ("日本語", None),
            ("___", None),
            ("", None),
            ("fn", None),
            ("class", None),
            ("a\"b", None),
        ];
        for (name, expected) in table {
            let got = crate_name_for(name).ok();
            assert_eq!(got.as_deref(), *expected, "{name:?}");
        }
    }

    /// Spec 61 § 2.1 `location`: an existing empty folder is a valid target; a non-empty one is not.
    #[test]
    fn an_empty_target_folder_is_used_and_a_full_one_refused() {
        let parent = temp_parent("empty_target");
        std::fs::create_dir(parent.join("my_game")).expect("empty folder");
        let request = NewProjectRequest {
            name: "My Game".into(),
            template: "blank".into(),
            parent: parent.clone(),
        };
        let created = create(&request).expect("an empty folder is free");
        assert!(created.root.join("Regia.toml").is_file());
        assert!(matches!(create(&request), Err(NewProjectError::Exists(_))));
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Spec 61 § 11: generated names avoid Windows reserved device names. The crate name is the
    /// project folder, and Windows cannot create a folder called `con` or `com1`.
    #[test]
    fn windows_device_names_are_refused() {
        for name in [
            "con", "CON", "Nul", "aux", "prn", " con ", "com0", "com1", "COM9", "lpt0", "lpt1", "LPT9",
        ] {
            assert!(
                matches!(crate_name_for(name), Err(NewProjectError::BadName(_))),
                "{name:?}"
            );
        }
    }

    /// Spec 61 § 9 and § 10 test 4: the same request writes the same bytes, for every template.
    #[test]
    fn every_template_writes_the_same_bytes_twice() {
        fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            let mut out = std::collections::BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).expect("read dir") {
                    let path = entry.expect("entry").path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        let rel = path.strip_prefix(root).expect("under root").to_path_buf();
                        out.insert(rel, std::fs::read(&path).expect("read"));
                    }
                }
            }
            out
        }
        for id in regia_templates::TEMPLATE_IDS {
            let a = temp_parent("det-a");
            let b = temp_parent("det-b");
            let request = |parent: &Path| NewProjectRequest {
                name: "Same Game".into(),
                template: (*id).to_owned(),
                parent: parent.to_path_buf(),
            };
            let first = create(&request(&a)).expect("first");
            let second = create(&request(&b)).expect("second");
            let (ta, tb) = (tree(&first.root), tree(&second.root));
            assert!(!ta.is_empty(), "{id}: wrote files");
            assert_eq!(
                ta.keys().collect::<Vec<_>>(),
                tb.keys().collect::<Vec<_>>(),
                "{id}: same file list"
            );
            for (rel, bytes) in &ta {
                assert!(tb.get(rel) == Some(bytes), "{id}: {} differs", rel.display());
            }
            let _ = std::fs::remove_dir_all(&a);
            let _ = std::fs::remove_dir_all(&b);
        }
    }

    #[test]
    fn failed_generation_leaves_no_partial_project_and_a_retry_succeeds() {
        let parent = regia_test_utils::unique_temp_dir("new_rollback");
        let result = create(&NewProjectRequest {
            name: "Hello {{oops}}".into(),
            template: "blank".into(),
            parent: parent.clone(),
        });
        assert!(result.is_err());
        let root = parent.join("hello_oops");
        assert!(!root.exists());
        std::fs::write(parent.join("user.bin"), b"user data").expect("user file");
        let result = write_project_files(
            &root,
            &[
                ("occupied", "file".into()),
                ("occupied/child", "cannot write".into()),
            ],
        );
        assert!(result.is_err());
        assert!(!root.exists());
        assert_eq!(
            std::fs::read(parent.join("user.bin")).expect("preserved user file"),
            b"user data"
        );
        write_project_files(&root, &[("Cargo.toml", "[package]".into())]).expect("retry");
        assert!(matches!(
            write_project_files(&root, &[]),
            Err(NewProjectError::Exists(_))
        ));
        assert_eq!(
            std::fs::read(root.join("Cargo.toml")).expect("existing project"),
            b"[package]"
        );
        std::fs::remove_dir_all(parent).expect("cleanup");
    }

    #[test]
    fn generated_names_are_valid_application_ids_before_writing_any_files() {
        let parent = regia_test_utils::unique_temp_dir("new_java_keywords");
        for name in ["Class", "Int", "Long", "Native", "Default", "_1"] {
            let result = create(&NewProjectRequest {
                name: name.into(),
                template: "blank".into(),
                parent: parent.clone(),
            });
            assert!(
                matches!(result, Err(NewProjectError::BadName(_))),
                "{name}: {result:?}"
            );
        }
        assert_eq!(std::fs::read_dir(&parent).expect("parent").count(), 0);
        std::fs::remove_dir_all(parent).expect("cleanup");
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
        let line = regia_core::engine_version::CURRENT.line();
        assert!(manifest.contains(&format!("engine = \"{line}\"")), "{manifest}");
        assert!(manifest.contains("startup_level = \"levels/main.level.ron\""));
        assert!(manifest.contains("# terrain_edits = true"), "{manifest}");
        assert!(created.root.join("assets/levels/main.level.ron").is_file());
        assert!(created.root.join("game/src/lib.rs").is_file());
        assert!(created.root.join("editor/src/main.rs").is_file());
        assert!(created.root.join("launcher/src/main.rs").is_file());
        assert!(created.root.join("android/src/lib.rs").is_file());
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml")).expect("game toml");
        assert!(game.contains("name = \"my_game\""), "{game}");
        assert!(game.contains("crates/regia\""), "path dep: {game}");
        assert!(!game.contains("{{"), "unsubstituted: {game}");
        assert!(
            game.contains("hot-reload = [\"dep:regia_dylib\""),
            "hot-reload feature: {game}"
        );
        assert!(game.contains("crates/regia_dylib\""), "dylib dep: {game}");
        assert!(game.contains("crates/regia_editor\""), "anchor dep: {game}");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        // A second create into the same folder is refused.
        let again = create(&NewProjectRequest {
            name: "My Game".into(),
            template: "blank".into(),
            parent,
        });
        assert!(again.is_err(), "the directory exists");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    #[test]
    fn third_person_template_writes_a_playable_starter() {
        let parent = temp_parent("third");
        let created = create(&NewProjectRequest {
            name: "Hero Quest".into(),
            template: "third_person".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "hero_quest");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("use_rapier_character_controller"), "{lib}");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        assert!(!lib.contains("{{"), "unsubstituted: {lib}");
        // The boom camera lives in the character module (ADR-2559).
        let character =
            std::fs::read_to_string(created.root.join("game/src/character.rs")).expect("character");
        assert!(character.contains("CameraMode::ThirdPerson"), "{character}");
        assert!(!character.contains("{{"), "unsubstituted: {character}");
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml")).expect("game toml");
        assert!(game.contains("regia_framework"), "framework dep: {game}");
        assert!(game.contains("crates/regia\""), "facade dep: {game}");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    #[test]
    fn unknown_templates_are_refused() {
        let err = create(&NewProjectRequest {
            name: "X".into(),
            template: "fps".into(),
            parent: std::env::temp_dir(),
        })
        .expect_err("tier");
        let message = err.to_string();
        assert!(message.contains("blank"), "{message}");
        assert!(message.contains("first_person"), "{message}");
        assert!(message.contains("third_person"), "{message}");
        assert!(message.contains("top_down"), "{message}");
        assert!(message.contains("quick"), "{message}");
    }

    #[test]
    fn top_down_template_writes_a_playable_starter() {
        let parent = temp_parent("top-down");
        let created = create(&NewProjectRequest {
            name: "Lantern Keep".into(),
            template: "top_down".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "lantern_keep");
        for file in [
            "Regia.toml",
            "Cargo.toml",
            ".gitignore",
            "game/src/lib.rs",
            "launcher/src/main.rs",
            "android/src/lib.rs",
            "editor/src/main.rs",
            "assets/levels/main.level.ron",
        ] {
            let text = std::fs::read_to_string(created.root.join(file)).unwrap_or_default();
            assert!(!text.contains("{{"), "{file}: unsubstituted: {text}");
        }
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("CameraMode::Overhead"), "{lib}");
        assert!(lib.contains("SaveFlowPlugin"), "{lib}");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        let launcher = std::fs::read_to_string(created.root.join("launcher/src/main.rs")).expect("launcher");
        assert!(launcher.contains("lantern_keep::Game"), "{launcher}");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    #[test]
    fn quick_template_writes_one_crate_with_one_main() {
        let parent = temp_parent("quick");
        let created = create(&NewProjectRequest {
            name: "Paddle Ball".into(),
            template: "quick".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "paddle_ball");
        let manifest = std::fs::read_to_string(created.root.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("name = \"paddle_ball\""), "{manifest}");
        assert!(manifest.contains("crates/regia\""), "path dep: {manifest}");
        assert!(
            !manifest.contains("[workspace]"),
            "one crate, not a workspace: {manifest}"
        );
        let main = std::fs::read_to_string(created.root.join("src/main.rs")).expect("main");
        assert!(main.contains("regia::quick::app(model)"), "{main}");
        assert!(
            main.contains("#[derive(Reflect"),
            "the model is reflected: {main}"
        );
        assert!(main.contains("Paddle Ball"), "{main}");
        assert!(!main.contains("{{"), "unsubstituted: {main}");
        assert!(!created.root.join("game").exists() && !created.root.join("Regia.toml").exists());
        let _ = std::fs::remove_dir_all(&created.root);
    }

    #[test]
    fn first_person_template_writes_a_first_person_game() {
        let parent = temp_parent("first_person");
        let created = create(&NewProjectRequest {
            name: "Walker".into(),
            template: "first_person".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "walker");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("FirstPerson"), "{lib}");
        assert!(lib.contains("REGIA:SCRIPTS"), "{lib}");
        let game_toml = std::fs::read_to_string(created.root.join("game/Cargo.toml")).expect("game toml");
        assert!(game_toml.contains("regia_framework"), "{game_toml}");
        assert!(!lib.contains("{{"), "unsubstituted: {lib}");
        let _ = std::fs::remove_dir_all(&created.root);
    }

    /// `regia new --template net-game` writes the dedicated server, the client and the shared game
    /// crate, every placeholder filled (ADR-2039).
    #[test]
    fn net_game_template_writes_server_client_and_game_crates() {
        let parent = temp_parent("net_game");
        let created = create(&NewProjectRequest {
            name: "Tag Yard".into(),
            template: "net-game".into(),
            parent: parent.clone(),
        })
        .expect("create");
        assert_eq!(created.crate_name, "tag_yard");
        for rel in [
            "Cargo.toml",
            "Regia.toml",
            "game/Cargo.toml",
            "game/src/lib.rs",
            "game/src/lobby.rs",
            "game/tests/e2e.rs",
            "server/Cargo.toml",
            "server/src/main.rs",
            "client/Cargo.toml",
            "client/src/main.rs",
        ] {
            let text = std::fs::read_to_string(created.root.join(rel)).unwrap_or_default();
            assert!(!text.is_empty(), "{rel} written");
            assert!(!text.contains("{{"), "{rel} has an unsubstituted placeholder");
        }
        let server = std::fs::read_to_string(created.root.join("server/Cargo.toml")).unwrap_or_default();
        assert!(server.contains("name = \"tag_yard_server\""), "{server}");
        assert!(
            server.contains(
                "tag_yard = { path = \"../game\", default-features = false, features = [\"server\"] }"
            ),
            "the server turns the game's default `client` feature off: {server}"
        );
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml")).unwrap_or_default();
        assert!(game.contains("default = [\"client\"]"), "{game}");
        let main = std::fs::read_to_string(created.root.join("server/src/main.rs")).unwrap_or_default();
        assert!(main.contains("use tag_yard::"), "{main}");
        let lib = std::fs::read_to_string(created.root.join("game/src/lib.rs")).unwrap_or_default();
        assert!(lib.contains("//! Tag Yard: the template networked game"), "{lib}");
        assert!(
            lib.contains("\"com.regiaengine.tag_yard\""),
            "the game id is the project's"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }
}
