// SPDX-License-Identifier: MPL-2.0
#![cfg(test)]
//! File locks (ADR-7864): the attributes matcher, the lock-list parser, and, with a fake
//! `git lfs` (a shim git program that records every `lfs` call and answers canned lock lists, so
//! no LFS server is needed): lock on edit, the save guard, the prompt's keyboard path, unlock after
//! commit, the no-LFS case and the agent methods over the dispatcher.

use super::*;

#[test]
fn attribute_patterns_follow_the_documented_rules() {
    let mut rules = LockRules::default();
    rules.add_text(
        "# art\n*.psd filter=lfs diff=lfs merge=lfs -text lockable\n\
         /assets/levels/*.level.ron lockable\n\
         **/textures/*.png lockable\n\
         assets/keep/*.psd -lockable\n\
         build/ lockable\n\
         *.[ab]in lockable\n",
        "",
    );
    assert!(rules.any());
    assert!(rules.is_lockable("art.psd"));
    assert!(
        rules.is_lockable("assets/deep/dir/art.psd"),
        "no slash: any depth"
    );
    assert!(rules.is_lockable("assets/levels/main.level.ron"));
    assert!(
        !rules.is_lockable("other/assets/levels/main.level.ron"),
        "anchored"
    );
    assert!(
        !rules.is_lockable("assets/levels/sub/main.level.ron"),
        "* stops at /"
    );
    assert!(
        rules.is_lockable("textures/a.png"),
        "**/ matches zero directories"
    );
    assert!(rules.is_lockable("a/b/textures/a.png"));
    assert!(
        !rules.is_lockable("assets/keep/art.psd"),
        "a later -lockable wins"
    );
    assert!(
        !rules.is_lockable("build/x"),
        "a directory pattern never matches a file"
    );
    assert!(rules.is_lockable("x/c.bin") && rules.is_lockable("a.ain"));
    assert!(!rules.is_lockable("c.cin"));
    // A project inside a larger repository: its own attributes are relative to its directory.
    let mut nested = LockRules::default();
    nested.add_text("/assets/*.bin lockable\n", "game/");
    assert!(nested.is_lockable("game/assets/c.bin"));
    assert!(!nested.is_lockable("assets/c.bin"));
    assert!(glob_match(b"a/**", b"a/b/c") && glob_match(b"a/**/b", b"a/b"));
    assert!(glob_match(b"\\*x", b"*x") && !glob_match(b"\\*x", b"ax"));
}

#[test]
fn lock_lists_parse_ours_theirs_and_plain_arrays() {
    let verified = br#"{"ours":[{"id":"1","path":"a.psd","owner":{"name":"Me"},"locked_at":"t1"}],
        "theirs":[{"id":2,"path":"b.psd","owner":{"name":"Jane Doe"},"locked_at":"t2"}]}"#;
    let locks = parse_locks_json(verified, "").expect("parse");
    assert_eq!(locks.len(), 2);
    assert!(locks[0].mine && locks[0].path == "a.psd" && locks[0].id == "1");
    assert!(!locks[1].mine && locks[1].owner == "Jane Doe" && locks[1].id == "2");
    let plain = br#"[{"id":"3","path":"c.psd","owner":{"name":"Me"}},{"id":"4","path":"d.psd","owner":{"name":"Jo"}}]"#;
    let locks = parse_locks_json(plain, "Me").expect("parse");
    assert_eq!(
        locks
            .iter()
            .map(|l| (l.path.as_str(), l.mine))
            .collect::<Vec<_>>(),
        vec![("c.psd", true), ("d.psd", false)]
    );
    assert!(parse_locks_json(b"", "").expect("empty").is_empty());
    assert_eq!(
        parse_locks_json(b"not json", "").expect_err("bad").code,
        "lfs_failed"
    );
}

#[test]
fn a_copy_keeps_every_extension_and_never_overwrites() {
    let dir = regia_test_utils::TempDir::new("vcs-lock-copy");
    let level = dir.join("main.level.ron");
    assert_eq!(copy_path(&level), dir.join("main (copy).level.ron"));
    std::fs::write(dir.join("main (copy).level.ron"), b"x").expect("write");
    assert_eq!(copy_path(&level), dir.join("main (copy 2).level.ron"));
    assert_eq!(copy_path(&dir.join("noext")), dir.join("noext (copy)"));
}

#[test]
fn every_lock_error_code_is_protocol_shaped_and_listed() {
    for code in ERROR_CODES {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    let page = std::fs::read_to_string(
        regia_test_utils::book::workspace_root().join("book/agents/revision-control.md"),
    )
    .expect("the book page");
    for code in ERROR_CODES {
        assert!(page.contains(&format!("`{code}`")), "{code} is on the book page");
    }
    let names: Vec<&str> = methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        vec!["editor.vcs_locks", "editor.vcs_lock", "editor.vcs_unlock"]
    );
}

// ---------------------------------------------------------------------------
// With the fake `git lfs` (unix: the shim is a shell script)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod with_shim {
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use crate::vcs::locks::*;

    /// Runs real git in `dir` (the fixture's own setup).
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs (the build hosts have git)");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The fake `git lfs`: a git program that appends every `lfs` call to `lfs-calls.log` and answers
    /// `version`, `locks` (the canned `locks.json`), `lock` and `unlock`; `no-lfs` makes `lfs` fail as
    /// on a machine without git LFS. Every other call runs the real git.
    const SHIM: &str = r#"#!/bin/sh
    here=$(dirname "$0")
    if [ "$1" = "lfs" ]; then
      shift
      echo "$*" >> "$here/lfs-calls.log"
      if [ -e "$here/no-lfs" ]; then echo "git: 'lfs' is not a git command." >&2; exit 1; fi
      case "$1" in
        version) echo "git-lfs/3.5.1 (test shim)";;
        locks) cat "$here/locks.json";;
        lock)
          for a in "$@"; do last="$a"; done
          if [ -e "$here/lock-refused" ]; then echo "Lock exists" >&2; exit 2; fi
          printf '{"id":"42","path":"%s","owner":{"name":"Fixture Author"},"locked_at":"2026-11-15T00:00:00Z"}\n' "$last";;
        unlock) echo '{"unlocked":true}';;
        *) echo "unknown lfs command $1" >&2; exit 2;;
      esac
      exit 0
    fi
    exec git "$@"
    "#;

    struct Fixture {
        repo: regia_test_utils::TempDir,
        shim_dir: regia_test_utils::TempDir,
    }

    impl Fixture {
        /// A repository with `.gitattributes` `attributes`, `assets/a.txt` and `assets/c.bin`
        /// committed, and the shim with an empty lock list.
        fn new(tag: &str, attributes: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let repo = regia_test_utils::TempDir::new(tag);
            git(&repo, &["init", "-q", "-b", "main"]);
            for (key, value) in [
                ("user.name", "Fixture Author"),
                ("user.email", "fixture@example.invalid"),
                ("commit.gpgsign", "false"),
                ("core.hooksPath", ".git/no-hooks"),
            ] {
                git(&repo, &["config", key, value]);
            }
            std::fs::create_dir_all(repo.join("assets")).expect("dirs");
            std::fs::write(repo.join("assets/a.txt"), b"one\n").expect("write");
            std::fs::write(repo.join("assets/c.bin"), [0u8, 1, 2]).expect("write");
            std::fs::write(repo.join(".gitattributes"), attributes).expect("write");
            std::fs::write(repo.join(".gitignore"), b"/.regia\n").expect("write");
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "first"]);
            let shim_dir = regia_test_utils::TempDir::new(&format!("{tag}-shim"));
            let shim = shim_dir.join("git-shim");
            std::fs::write(&shim, SHIM).expect("shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let fixture = Self { repo, shim_dir };
            fixture.set_locks(r#"{"ours":[],"theirs":[]}"#);
            fixture
        }

        fn shim(&self) -> PathBuf {
            self.shim_dir.join("git-shim")
        }

        fn set_locks(&self, json: &str) {
            std::fs::write(self.shim_dir.join("locks.json"), json).expect("locks");
        }

        /// The recorded `lfs` calls, one per line.
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.shim_dir.join("lfs-calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn calls_of(&self, op: &str) -> usize {
            self.calls()
                .iter()
                .filter(|c| c.split(' ').next() == Some(op))
                .count()
        }

        /// A world with the project open and the shim as the git program; `lock_on_edit` set.
        fn world(&self, lock_on_edit: &str) -> World {
            let mut world = World::new();
            world.insert_resource(crate::agent_ext::EditorProject {
                root: Some(self.repo.to_path_buf()),
            });
            world.insert_resource(prefs(&self.shim(), lock_on_edit));
            world
        }
    }

    fn prefs(shim: &Path, lock_on_edit: &str) -> crate::editor_prefs::EditorPreferences {
        use crate::editor_prefs::{EditorPreferences, PrefValue};
        let mut prefs = EditorPreferences::default();
        prefs
            .set(
                crate::vcs::GIT_COMMAND_PREF,
                &PrefValue::Text(shim.display().to_string()),
            )
            .expect("git pref");
        prefs
            .set(LOCK_ON_EDIT_PREF, &PrefValue::Text(lock_on_edit.to_string()))
            .expect("lock pref");
        prefs
    }

    /// The files the test editor holds unsaved edits of.
    #[derive(Resource, Default)]
    struct TestDirty(Vec<PathBuf>);

    fn test_dirty(world: &World) -> Vec<crate::doc_autosave::DocKey> {
        world
            .get_resource::<TestDirty>()
            .map(|d| {
                d.0.iter()
                    .map(|p| crate::doc_autosave::DocKey::new("lock_test", Some(p)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn no_snapshots(_: &World) -> Vec<crate::doc_autosave::DocSnapshot> {
        Vec::new()
    }

    fn no_restore(_: &mut World, _: &crate::doc_autosave::DocKey, _: &[u8]) -> Result<(), String> {
        Ok(())
    }

    /// Registers the test editor (an asset editor of the document autosave registry).
    fn register_test_editor(world: &mut World) {
        world.insert_resource(TestDirty::default());
        world
            .get_resource_or_insert_with(crate::doc_autosave::DocAutosaveRegistry::default)
            .register(crate::doc_autosave::DocProvider {
                kind: "lock_test",
                dirty: test_dirty,
                snapshot: no_snapshots,
                restore: no_restore,
            });
    }

    const LOCKABLE_BIN: &str = "*.bin filter=lfs diff=lfs merge=lfs -text lockable\n";

    /// Acceptance: editing a lockable asset calls `git lfs lock` once (however many frames the edit
    /// stays unsaved); editing a file that is not lockable calls it never.
    #[test]
    fn editing_a_lockable_asset_locks_it_once() {
        let fx = Fixture::new("vcs-lock-edit", LOCKABLE_BIN);
        let mut world = fx.world("always");
        register_test_editor(&mut world);
        tick_and_wait(&mut world);
        assert!(
            world.resource::<VcsLocks>().available(),
            "{:?}",
            world.resource::<VcsLocks>().support
        );
        assert_eq!(fx.calls_of("lock"), 0, "nothing edited yet: {:?}", fx.calls());
        // The non-lockable file first: no lock, no lock-list read for it.
        world.resource_mut::<TestDirty>().0 = vec![fx.repo.join("assets/a.txt")];
        for _ in 0..3 {
            tick_and_wait(&mut world);
        }
        assert_eq!(fx.calls_of("lock"), 0, "{:?}", fx.calls());
        // The first dirtying edit of the lockable file locks it once.
        world.resource_mut::<TestDirty>().0 =
            vec![fx.repo.join("assets/a.txt"), fx.repo.join("assets/c.bin")];
        for _ in 0..5 {
            tick_and_wait(&mut world);
        }
        assert_eq!(fx.calls_of("lock"), 1, "{:?}", fx.calls());
        assert!(
            fx.calls().contains(&"lock --json -- assets/c.bin".to_string()),
            "{:?}",
            fx.calls()
        );
        let locks = world.resource::<VcsLocks>();
        assert_eq!(locks.lock_calls, 1);
        assert!(locks.lock_of("assets/c.bin").is_some_and(|l| l.mine));
        assert_eq!(badge(&world, "assets/c.bin").map(|b| b.0), Some("L"));
        assert_eq!(
            decorate_row(&world, "assets/c.bin", "c.bin".into()),
            "c.bin   [Locked by you]"
        );
        // Saved, then edited again: already the user's lock, no second call.
        world.resource_mut::<TestDirty>().0.clear();
        tick_and_wait(&mut world);
        world.resource_mut::<TestDirty>().0 = vec![fx.repo.join("assets/c.bin")];
        tick_and_wait(&mut world);
        assert_eq!(fx.calls_of("lock"), 1, "{:?}", fx.calls());
    }

    /// `ask` (the default): the first edit opens the prompt; Tab to Lock and Enter locks the file
    /// once; every control is named.
    #[test]
    fn the_lock_prompt_locks_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let fx = Fixture::new("vcs-lock-prompt", LOCKABLE_BIN);
        let mut world = fx.world("ask");
        register_test_editor(&mut world);
        tick_and_wait(&mut world);
        world.resource_mut::<TestDirty>().0 = vec![fx.repo.join("assets/c.bin")];
        tick_and_wait(&mut world);
        tick_and_wait(&mut world);
        assert_eq!(
            world
                .resource::<VcsLocks>()
                .prompts
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![LockPrompt::LockOnEdit("assets/c.bin".into())],
            "one prompt however many ticks"
        );
        assert_eq!(fx.calls_of("lock"), 0, "ask never locks by itself");
        let ctx = egui::Context::default();
        let size = [600.0, 400.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let prompt = world.resource::<VcsLocks>().prompts.front().cloned();
                if let Some(prompt) = prompt {
                    draw_prompt_and_apply(ui, &mut world, &prompt);
                }
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        tab_to(&ctx, size, "Lock", false, 10, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        assert_eq!(fx.calls_of("lock"), 1, "{:?}", fx.calls());
        let locks = world.resource::<VcsLocks>();
        assert!(locks.prompts.is_empty(), "{:?}", locks.prompts);
        assert_eq!(locks.notice, Some((false, "Locked 1 file(s)".to_string())));
    }

    /// Acceptance: a file locked by someone else is never locked over, warns on the first edit, and
    /// blocks the level save with `locked_by_other`; "Save anyway as a copy" writes a new file and
    /// leaves the locked one untouched.
    #[test]
    fn a_level_locked_by_someone_else_blocks_the_save() {
        use regia_core::id::Guid;
        use regia_scene::level::open_level;
        use regia_scene::prelude::{Level, LevelId, LevelLibrary, LoadMode};
        let fx = Fixture::new("vcs-lock-blocked", "*.level.ron lockable\n");
        let level_file = fx.repo.join("assets/main.level.ron");
        std::fs::write(&level_file, b"(format_version: 1, entities: [])\n").expect("level");
        fx.set_locks(
            r#"{"ours":[],"theirs":[{"id":"7","path":"assets/main.level.ron","owner":{"name":"Jane Doe"},"locked_at":"2026-11-14T10:00:00Z"}]}"#,
        );
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, "(format_version: 1, entities: [])").expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let world = app.world_mut();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(fx.repo.to_path_buf()),
        });
        world.insert_resource(prefs(&fx.shim(), "always"));
        let open = crate::level_io::OpenLevel {
            id,
            path: Some(level_file.clone()),
        };
        world.insert_resource(open.clone());
        let mut undo = crate::command::UndoStack::new();
        undo.mark_dirty();
        world.insert_resource(undo);
        tick_and_wait(world);
        tick_and_wait(world);
        // The first edit read the list and warned; nothing was locked over Jane's lock.
        assert_eq!(fx.calls_of("lock"), 0, "{:?}", fx.calls());
        assert!(
            world
                .resource::<VcsLocks>()
                .prompts
                .contains(&LockPrompt::LockedByOther {
                    path: "assets/main.level.ron".into(),
                    owner: "Jane Doe".into(),
                })
        );
        assert_eq!(badge(world, "assets/main.level.ron").map(|b| b.0), Some("X"));
        let refused = lock_paths(world, &["assets/main.level.ron".to_string()]).expect_err("refused");
        assert_eq!(refused.code, "locked_by_other");
        assert_eq!(fx.calls_of("lock"), 0);
        // The save is blocked with the documented code; the file is untouched.
        let before = std::fs::read(&level_file).expect("read");
        let error = crate::level_io::save_level_and_terrain(world, &open, &level_file).expect_err("blocked");
        assert!(
            error.contains("locked_by_other") && error.contains("Jane Doe"),
            "{error}"
        );
        assert_eq!(std::fs::read(&level_file).expect("read"), before);
        assert_eq!(
            guard_save(world, &level_file).expect_err("guard").code,
            "locked_by_other"
        );
        // The prompt offers the copy: a new file next to it, the locked one unchanged.
        world
            .resource_mut::<VcsLocks>()
            .prompts
            .retain(|p| matches!(p, LockPrompt::SaveBlocked { .. }));
        world.resource_mut::<VcsLocks>().pending.push(LockOp::SaveCopy);
        apply(world);
        let copy = fx.repo.join("assets/main (copy).level.ron");
        assert!(copy.is_file(), "{:?}", world.resource::<VcsLocks>().notice);
        assert_eq!(std::fs::read(&level_file).expect("read"), before);
        assert!(guard_save(world, &copy).is_ok(), "the copy is not locked");
        // The documents' save-all path refuses a dirty document someone else locks.
        register_test_editor(world);
        world.resource_mut::<TestDirty>().0 = vec![level_file.clone()];
        assert!(
            guard_documents(world)
                .expect_err("blocked")
                .contains("locked_by_other")
        );
    }

    /// Acceptance: without LFS locking nothing changes: no lockable patterns, or no git LFS, means
    /// no lock call, no prompt, no badge and no blocked save; the methods answer
    /// `locking_unavailable`.
    #[test]
    fn without_lfs_locking_nothing_changes() {
        // Lockable patterns, but git LFS is missing.
        let fx = Fixture::new("vcs-lock-no-lfs", LOCKABLE_BIN);
        std::fs::write(fx.shim_dir.join("no-lfs"), b"").expect("marker");
        fx.set_locks(
            r#"{"ours":[],"theirs":[{"id":"7","path":"assets/c.bin","owner":{"name":"Jane Doe"}}]}"#,
        );
        let mut world = fx.world("always");
        register_test_editor(&mut world);
        world.resource_mut::<TestDirty>().0 = vec![fx.repo.join("assets/c.bin")];
        for _ in 0..3 {
            tick_and_wait(&mut world);
        }
        let locks = world.resource::<VcsLocks>();
        assert!(
            matches!(&locks.support, LockSupport::Unavailable(why) if why.contains("git LFS")),
            "{:?}",
            locks.support
        );
        assert!(locks.prompts.is_empty() && locks.locks.is_empty());
        assert_eq!(fx.calls_of("lock") + fx.calls_of("locks"), 0, "{:?}", fx.calls());
        assert!(guard_save(&world, &fx.repo.join("assets/c.bin")).is_ok());
        assert!(guard_documents(&world).is_ok());
        assert_eq!(badge(&world, "assets/c.bin"), None);
        let error = lock_paths(&mut world, &["assets/c.bin".to_string()]).expect_err("unavailable");
        assert_eq!(error.code, "locking_unavailable");
        let answer = locks_json(&world, &["assets/c.bin".to_string()]);
        assert_eq!(answer["support"], "unavailable");
        assert_eq!(answer["query"][0]["lockable"], false);

        // git LFS works, but no path is lockable: LFS is never even asked.
        let plain = Fixture::new(
            "vcs-lock-no-patterns",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        let mut world = plain.world("always");
        register_test_editor(&mut world);
        world.resource_mut::<TestDirty>().0 = vec![plain.repo.join("assets/c.bin")];
        for _ in 0..3 {
            tick_and_wait(&mut world);
        }
        assert!(plain.calls().is_empty(), "{:?}", plain.calls());
        assert!(
            matches!(&world.resource::<VcsLocks>().support, LockSupport::Unavailable(why) if why.contains("lockable")),
            "{:?}",
            world.resource::<VcsLocks>().support
        );
    }

    /// After a commit the committed files the user locks are offered for unlock; Unlock all mine
    /// releases them (never with `--force`); someone else's lock is refused.
    #[test]
    fn unlock_after_commit_and_unlock_all_mine() {
        let fx = Fixture::new("vcs-lock-unlock", LOCKABLE_BIN);
        let mut world = fx.world("never");
        tick_and_wait(&mut world);
        lock_paths(&mut world, &["assets/c.bin".to_string()]).expect("lock");
        std::fs::write(fx.repo.join("assets/c.bin"), [9u8, 9, 9]).expect("edit");
        crate::vcs::agent::stage(&mut world, &["assets/c.bin".to_string()]).expect("stage");
        crate::vcs::agent::commit(&mut world, "binary edit", false).expect("commit");
        let locks = world.resource::<VcsLocks>();
        assert_eq!(locks.unlock_offer, vec!["assets/c.bin".to_string()]);
        assert!(locks.window_open, "the File Locks window offers the unlock");
        // Someone else's lock is never released.
        world.resource_mut::<VcsLocks>().locks.push(LockInfo {
            path: "assets/z.bin".into(),
            id: "9".into(),
            owner: "Jane Doe".into(),
            mine: false,
            locked_at: String::new(),
        });
        let refused = unlock_paths(&mut world, &["assets/z.bin".to_string()]).expect_err("theirs");
        assert_eq!(refused.code, "not_locked_by_you");
        let done = unlock_all_mine(&mut world).expect("unlock all mine");
        assert_eq!(done, vec!["assets/c.bin".to_string()]);
        assert_eq!(fx.calls_of("unlock"), 1, "{:?}", fx.calls());
        assert!(!fx.calls().iter().any(|c| c.contains("--force")));
        let locks = world.resource::<VcsLocks>();
        assert!(locks.unlock_offer.is_empty());
        assert!(locks.lock_of("assets/c.bin").is_none());
        assert!(locks.lock_of("assets/z.bin").is_some(), "Jane's lock stays");
    }

    /// The File Locks window from the keyboard: Tab to Unlock all mine, Enter releases the user's
    /// lock once; every control is named.
    #[test]
    fn the_file_locks_window_unlocks_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let fx = Fixture::new("vcs-lock-window-keys", LOCKABLE_BIN);
        let mut world = fx.world("never");
        tick_and_wait(&mut world);
        lock_paths(&mut world, &["assets/c.bin".to_string()]).expect("lock");
        let ctx = egui::Context::default();
        let size = [600.0, 500.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| draw_and_apply(ui, &mut world));
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        tab_to(&ctx, size, "Unlock all mine", false, 10, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        assert_eq!(fx.calls_of("unlock"), 1, "{:?}", fx.calls());
        let locks = world.resource::<VcsLocks>();
        assert!(locks.lock_of("assets/c.bin").is_none());
        assert_eq!(locks.notice, Some((false, "Unlocked 1 file(s)".to_string())));
    }

    /// A second UI pass in one frame does not repeat the File Locks window's op.
    #[test]
    fn a_second_ui_pass_does_not_lock_twice() {
        let fx = Fixture::new("vcs-lock-two-pass", LOCKABLE_BIN);
        let mut world = fx.world("never");
        tick_and_wait(&mut world);
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            for _ in 0..2 {
                world
                    .resource_mut::<VcsLocks>()
                    .pending
                    .push(LockOp::Lock(vec!["assets/c.bin".into()]));
                draw_and_apply(ui, &mut world);
            }
        });
        out.textures_delta.clear();
        assert_eq!(world.resource::<VcsLocks>().ops_applied, 1);
        assert_eq!(fx.calls_of("lock"), 1, "{:?}", fx.calls());
    }

    /// The methods over the real dispatcher (schema rows, dispatch arms), and `editor.save_level`
    /// refusing a level someone else locks with `locked_by_other`.
    #[test]
    fn the_lock_methods_answer_through_the_agent_dispatcher() {
        let fx = Fixture::new("vcs-lock-dispatch", "*.bin lockable\n*.level.ron lockable\n");
        fx.set_locks(
            r#"{"ours":[],"theirs":[{"id":"7","path":"assets/main.level.ron","owner":{"name":"Jane Doe"}}]}"#,
        );
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(fx.repo.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.insert_resource(crate::agent_ext::EditorProject {
            root: Some(fx.repo.to_path_buf()),
        });
        app.insert_resource(prefs(&fx.shim(), "ask"));
        app.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::prelude::LevelId::default(),
            path: Some(fx.repo.join("assets/main.level.ron")),
        });
        app.finish_plugins();
        let (sender, inbox) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(inbox));
        let mut call = |method: &str, params: Value| {
            let (reply, answer) = crossbeam_channel::bounded(1);
            sender
                .send(regia_agent::server::Inbound {
                    request: regia_agent::protocol::Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: method.into(),
                        params,
                    },
                    reply,
                })
                .expect("agent inbox");
            app.update();
            answer.try_recv().expect("dispatch answers in one frame")
        };
        let ok = |response: regia_agent::protocol::Response| {
            assert!(response.error.is_none(), "{:?}", response.error);
            response.result.expect("result")
        };
        let state = ok(call(
            "editor.vcs_locks",
            json!({ "paths": ["assets/c.bin", "assets/a.txt"] }),
        ));
        assert_eq!(state["support"], "available", "{state}");
        assert_eq!(state["theirs"], json!(["assets/main.level.ron"]));
        assert_eq!(state["query"][0]["lockable"], true);
        assert_eq!(state["query"][1]["lockable"], false);
        let locked = ok(call("editor.vcs_lock", json!({ "paths": ["assets/c.bin"] })));
        assert_eq!(locked["locked"][0]["path"], "assets/c.bin");
        assert_eq!(locked["locked"][0]["mine"], true);
        let again = ok(call("editor.vcs_lock", json!({ "paths": ["assets/c.bin"] })));
        assert_eq!(again["locked"][0]["path"], "assets/c.bin");
        assert_eq!(fx.calls_of("lock"), 1, "a lock you hold is not asked for again");
        let refused = call("editor.vcs_lock", json!({ "paths": ["assets/a.txt"] }));
        assert_eq!(error_code(&refused), "not_lockable");
        let blocked = call("editor.save_level", json!({}));
        assert_eq!(error_code(&blocked), "locked_by_other", "{:?}", blocked.error);
        let unlocked = ok(call("editor.vcs_unlock", json!({ "all_mine": true })));
        assert_eq!(unlocked["unlocked"], json!(["assets/c.bin"]));
    }

    fn error_code(response: &regia_agent::protocol::Response) -> String {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }
}
