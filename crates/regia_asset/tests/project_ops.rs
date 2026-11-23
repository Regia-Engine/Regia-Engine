// SPDX-License-Identifier: MPL-2.0
//! Move/rename a project asset and keep references valid (spec 12 § 9, M11).

mod tests {
    use std::path::{Path, PathBuf};

    use regia_asset::meta_path_for;
    use regia_asset::project_ops::{
        MoveError, find_path_referencers, find_referencers, move_asset, rewrite_path_references,
    };
    use regia_asset::{AssetId, META_FORMAT_VERSION, MetaSidecar};

    fn temp_project(tag: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(&format!("move-{tag}"));
        std::fs::create_dir_all(dir.join("assets")).expect("mkdir");
        dir
    }

    /// A source file plus its meta sidecar; returns the asset id.
    fn source_with_meta(root: &Path, rel: &str, importer: &str) -> AssetId {
        let full = root.join("assets").join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        std::fs::write(&full, b"asset").expect("write source");
        let id = AssetId::new_random();
        let meta = MetaSidecar {
            format_version: META_FORMAT_VERSION,
            id,
            importer: importer.to_string(),
            settings: None,
            sub_assets: Vec::new(),
            provenance: None,
        };
        std::fs::write(meta_path_for(&full), ron::to_string(&meta).expect("meta ron")).expect("write meta");
        id
    }

    #[test]
    fn rename_keeps_the_asset_id_and_fixes_path_references() {
        let root = temp_project("rename");
        let id = source_with_meta(&root, "textures/wood.png", "png");
        // A level references the texture by path; a material references it by guid (the form
        // saved instances store); a "mytextures" decoy must not be touched (whole-path rule).
        std::fs::write(
            root.join("assets/level.ron"),
            format!(
                "(name: \"level\",\n texture: \"textures/wood.png\",\n other: \"textures/redwood.png\",\n by_id: \"asset:{id}\",\n)"
            ),
        )
        .expect("write level");
        std::fs::write(root.join("assets/stuff.ron"), "mytextures/wood.png and more").expect("write stuff");

        let outcome = move_asset(&root, "textures/wood.png", "textures/oak.png").expect("move");

        assert_eq!(outcome.fixed_referencers, vec!["assets/level.ron".to_string()]);
        // Source and sidecar moved; the id is unchanged.
        assert!(!root.join("assets/textures/wood.png").exists());
        assert!(root.join("assets/textures/oak.png").exists());
        let meta = MetaSidecar::from_ron(
            &std::fs::read(meta_path_for(&root.join("assets/textures/oak.png"))).expect("meta"),
        )
        .expect("parse meta");
        assert_eq!(meta.id, id);
        // Path reference rewritten; decoys and the guid reference untouched.
        let level = std::fs::read_to_string(root.join("assets/level.ron")).expect("level");
        assert!(level.contains("texture: \"textures/oak.png\""), "{level}");
        assert!(level.contains("other: \"textures/redwood.png\""), "{level}");
        assert!(level.contains(&format!("by_id: \"asset:{id}\"")), "{level}");
        let stuff = std::fs::read_to_string(root.join("assets/stuff.ron")).expect("stuff");
        assert_eq!(stuff, "mytextures/wood.png and more");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn moving_a_directory_rewrites_references_under_it() {
        let root = temp_project("dir");
        source_with_meta(&root, "props/rock.prefab.ron", "prefab_ron");
        source_with_meta(&root, "props/stick.prefab.ron", "prefab_ron");
        std::fs::write(
            root.join("assets/map.ron"),
            "(rock: \"props/rock.prefab.ron\", stick: \"props/stick.prefab.ron\")",
        )
        .expect("write map");

        let outcome = move_asset(&root, "props", "models/props").expect("move dir");

        assert_eq!(outcome.fixed_referencers, vec!["assets/map.ron".to_string()]);
        assert!(root.join("assets/models/props/rock.prefab.ron").exists());
        assert!(root.join("assets/models/props/rock.prefab.ron.meta").exists());
        assert!(!root.join("assets/props").exists());
        let map = std::fs::read_to_string(root.join("assets/map.ron")).expect("map");
        assert!(map.contains("models/props/rock.prefab.ron"), "{map}");
        assert!(map.contains("models/props/stick.prefab.ron"), "{map}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_move_back_is_the_undo_and_restores_every_reference() {
        let root = temp_project("undo");
        let id = source_with_meta(&root, "a/thing.png", "png");
        std::fs::write(root.join("assets/use.ron"), "(texture: \"a/thing.png\")").expect("write user");

        move_asset(&root, "a/thing.png", "b/thing.png").expect("move");
        move_asset(&root, "b/thing.png", "a/thing.png").expect("undo move");

        assert!(root.join("assets/a/thing.png").exists());
        let meta = MetaSidecar::from_ron(
            &std::fs::read(meta_path_for(&root.join("assets/a/thing.png"))).expect("meta"),
        )
        .expect("parse meta");
        assert_eq!(meta.id, id, "the undo must not change the asset id");
        let user = std::fs::read_to_string(root.join("assets/use.ron")).expect("user");
        assert_eq!(user, "(texture: \"a/thing.png\")");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn move_refuses_a_missing_source_or_an_existing_destination() {
        let root = temp_project("refuse");
        source_with_meta(&root, "one.png", "png");
        source_with_meta(&root, "two.png", "png");

        assert!(matches!(
            move_asset(&root, "missing.png", "three.png"),
            Err(MoveError::SourceMissing(_))
        ));
        assert!(matches!(
            move_asset(&root, "one.png", "two.png"),
            Err(MoveError::DestinationExists(_))
        ));
        // A refused move changed nothing.
        assert!(root.join("assets/one.png").exists());
        assert!(root.join("assets/two.png").exists());
        assert!(!root.join("assets/three.png").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn move_refuses_paths_that_leave_assets() {
        let root = temp_project("invalid");
        source_with_meta(&root, "one.png", "png");

        for bad in ["../one.png", "a//b.png", "a/./b.png", "a\\b.png", "/abs.png", ""] {
            assert!(
                matches!(move_asset(&root, "one.png", bad), Err(MoveError::InvalidPath(_))),
                "{bad:?} must be rejected"
            );
        }
        assert!(root.join("assets/one.png").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_path_replaces_only_whole_path_mentions() {
        let rewritten = rewrite_path_references(
            "a \"props/rock.prefab.ron\" b redrock.prefab.ron c props/rock.prefab.ron.d e",
            "props/rock.prefab.ron",
            "props/stone.prefab.ron",
        )
        .expect("changed");
        assert_eq!(
            rewritten,
            "a \"props/stone.prefab.ron\" b redrock.prefab.ron c props/rock.prefab.ron.d e"
        );
        assert!(rewrite_path_references("nothing here", "a.png", "b.png").is_none());
        assert!(rewrite_path_references("a.png", "a.png", "a.png").is_none());
    }

    // ---- review sprint-rev-kimi-9 (assets-toc-k) findings 1-3, 8, 10 ----

    /// Finding 1: `/` is a path character, so renaming `assets/rock.glb` must not rewrite
    /// `props/rock.glb`, a different file that happens to end with the same name.
    #[test]
    fn rename_leaves_a_same_named_file_in_another_folder_alone() {
        let root = temp_project("same-name");
        source_with_meta(&root, "rock.glb", "gltf");
        source_with_meta(&root, "props/rock.glb", "gltf");
        std::fs::write(
            root.join("assets/map.ron"),
            "(a: \"props/rock.glb\", b: \"rock.glb\", c: \"assets/rock.glb\", d: \"assets/props/rock.glb\")",
        )
        .expect("write map");

        assert_eq!(
            rewrite_path_references("(a: \"props/rock.glb\")", "rock.glb", "stone.glb"),
            None,
            "props/rock.glb is not a mention of rock.glb"
        );
        let outcome = move_asset(&root, "rock.glb", "stone.glb").expect("move");

        assert_eq!(outcome.fixed_referencers, vec!["assets/map.ron".to_string()]);
        let map = std::fs::read_to_string(root.join("assets/map.ron")).expect("map");
        assert_eq!(
            map,
            "(a: \"props/rock.glb\", b: \"stone.glb\", c: \"assets/stone.glb\", d: \"assets/props/rock.glb\")"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 2: moving a folder rewrites paths under it, never a RON field name or a plain
    /// string that only equals the folder name.
    #[test]
    fn moving_a_folder_leaves_field_names_and_tag_strings_alone() {
        let root = temp_project("field-names");
        source_with_meta(&root, "props/rock.glb", "gltf");
        std::fs::write(
            root.join("assets/map.ron"),
            "(props: [], tag: \"props\", mesh: \"props/rock.glb\", full: \"assets/props/rock.glb\", dir: \"assets/props\")",
        )
        .expect("write map");
        std::fs::write(
            root.join("assets/settings.ron"),
            "(props: [1, 2], kind: \"props\")",
        )
        .expect("write settings");

        assert_eq!(
            find_referencers(&root, "assets/props"),
            vec!["assets/map.ron".to_string()],
            "a field or tag named like the folder is not a referencer"
        );
        let outcome = move_asset(&root, "props", "models").expect("move dir");

        assert_eq!(outcome.fixed_referencers, vec!["assets/map.ron".to_string()]);
        let map = std::fs::read_to_string(root.join("assets/map.ron")).expect("map");
        assert_eq!(
            map,
            "(props: [], tag: \"props\", mesh: \"models/rock.glb\", full: \"assets/models/rock.glb\", dir: \"assets/models\")"
        );
        let settings = std::fs::read_to_string(root.join("assets/settings.ron")).expect("settings");
        assert_eq!(settings, "(props: [1, 2], kind: \"props\")");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 3: moving a folder into itself is refused up front and changes nothing (it used to
    /// rewrite the referencers, then fail the rename and leave them pointing at nothing).
    #[test]
    fn moving_a_folder_into_itself_is_refused_and_changes_nothing() {
        let root = temp_project("into-itself");
        source_with_meta(&root, "props/rock.glb", "gltf");
        std::fs::write(root.join("assets/map.ron"), "(mesh: \"props/rock.glb\")").expect("write map");

        let result = move_asset(&root, "props", "props/sub");

        assert!(matches!(result, Err(MoveError::InvalidPath(_))), "{result:?}");
        assert!(root.join("assets/props/rock.glb").is_file());
        assert_eq!(
            std::fs::read_to_string(root.join("assets/map.ron")).expect("map"),
            "(mesh: \"props/rock.glb\")"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 3: a destination whose `.meta` sidecar is taken is refused before anything
    /// changes (the sidecar rename used to fail after the source had moved, leaving it behind).
    #[test]
    fn a_taken_sidecar_destination_is_refused_before_anything_changes() {
        let root = temp_project("meta-taken");
        let id = source_with_meta(&root, "rock.glb", "gltf");
        std::fs::write(root.join("assets/map.ron"), "(mesh: \"rock.glb\")").expect("write map");
        // Something already sits where the moved sidecar would go.
        std::fs::create_dir_all(root.join("assets/stone.glb.meta/keep")).expect("blocker");

        let result = move_asset(&root, "rock.glb", "stone.glb");

        assert!(
            matches!(result, Err(MoveError::DestinationExists(_))),
            "{result:?}"
        );
        assert!(root.join("assets/rock.glb").is_file(), "the source stays");
        assert!(!root.join("assets/stone.glb").exists());
        let meta = MetaSidecar::from_ron(
            &std::fs::read(meta_path_for(&root.join("assets/rock.glb"))).expect("meta"),
        )
        .expect("parse meta");
        assert_eq!(meta.id, id, "the sidecar stays with the source");
        assert_eq!(
            std::fs::read_to_string(root.join("assets/map.ron")).expect("map"),
            "(mesh: \"rock.glb\")"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 3: when the rename itself fails (here: a read-only destination folder), the
    /// referencer rewrites are rolled back, so the move is all-or-nothing.
    #[cfg(unix)]
    #[test]
    fn a_failed_rename_rolls_back_the_reference_rewrites() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_project("rollback");
        source_with_meta(&root, "props/rock.glb", "gltf");
        let level = "(mesh: \"props/rock.glb\", other: \"assets/props/rock.glb\")";
        std::fs::write(root.join("assets/map.ron"), level).expect("write map");
        let locked = root.join("assets/locked");
        std::fs::create_dir_all(&locked).expect("locked dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        // A privileged user (root in a container) can write anyway; then the rename cannot fail.
        let probe = locked.join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            eprintln!("skipped: this user can write into a read-only folder");
            let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let result = move_asset(&root, "props/rock.glb", "locked/rock.glb");

        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        assert!(matches!(result, Err(MoveError::Io { .. })), "{result:?}");
        assert!(root.join("assets/props/rock.glb").is_file(), "the source stays");
        assert!(
            root.join("assets/props/rock.glb.meta").is_file(),
            "the sidecar stays"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("assets/map.ron")).expect("map"),
            level,
            "the referencer rewrite is rolled back"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 8: the referencer walk does not follow a symlinked folder, so a move never
    /// rewrites a `.ron` file outside the project (or loops on a link cycle).
    #[cfg(unix)]
    #[test]
    fn the_referencer_walk_does_not_follow_symlinked_folders() {
        let root = temp_project("symlink-walk");
        let outside = regia_test_utils::unique_temp_dir("move-symlink-outside");
        source_with_meta(&root, "rock.glb", "gltf");
        std::fs::write(outside.join("elsewhere.ron"), "(mesh: \"rock.glb\")").expect("outside ron");
        std::os::unix::fs::symlink(&outside, root.join("assets/linked")).expect("dir link");
        std::os::unix::fs::symlink(root.join("assets"), root.join("assets/loop")).expect("loop link");

        assert!(find_referencers(&root, "assets/rock.glb").is_empty());
        let outcome = move_asset(&root, "rock.glb", "stone.glb").expect("move");

        assert!(
            outcome.fixed_referencers.is_empty(),
            "{:?}",
            outcome.fixed_referencers
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("elsewhere.ron")).expect("outside ron"),
            "(mesh: \"rock.glb\")",
            "a file outside the project is never rewritten"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Finding 10: a case-only rename (`Rock.png` to `rock.png`) is a rename, not a taken
    /// destination, on case-insensitive file systems (Windows, macOS) too. On a case-sensitive
    /// file system this passes on the old code as well; the Mac and Windows runs are the
    /// fail-before.
    #[test]
    fn a_case_only_rename_is_allowed() {
        let root = temp_project("case-only");
        let id = source_with_meta(&root, "Rock.png", "png");
        std::fs::write(root.join("assets/map.ron"), "(texture: \"Rock.png\")").expect("write map");

        let outcome = move_asset(&root, "Rock.png", "rock.png").expect("case-only rename");

        assert_eq!(outcome.fixed_referencers, vec!["assets/map.ron".to_string()]);
        let names: Vec<String> = std::fs::read_dir(root.join("assets"))
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"rock.png".to_string()), "{names:?}");
        assert!(!names.contains(&"Rock.png".to_string()), "{names:?}");
        assert!(names.contains(&"rock.png.meta".to_string()), "{names:?}");
        let meta = MetaSidecar::from_ron(
            &std::fs::read(meta_path_for(&root.join("assets/rock.png"))).expect("meta"),
        )
        .expect("parse meta");
        assert_eq!(meta.id, id);
        assert_eq!(
            std::fs::read_to_string(root.join("assets/map.ron")).expect("map"),
            "(texture: \"rock.png\")"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rewrite only edits string literals: comments, char literals and escaped quotes do not
    /// confuse the scan, and a comment that names the path is left as written.
    #[test]
    fn rewrite_edits_string_literals_only() {
        let text = "// it's \"rock.glb\" in a comment\n(c: '\"', a: \"rock.glb\", /* \"rock.glb\" */ b: \"say \\\"rock.glb\\\"\", d: r#\"rock.glb\"#)";
        let rewritten = rewrite_path_references(text, "rock.glb", "stone.glb").expect("changed");
        assert_eq!(
            rewritten,
            "// it's \"rock.glb\" in a comment\n(c: '\"', a: \"stone.glb\", /* \"rock.glb\" */ b: \"say \\\"rock.glb\\\"\", d: r#\"stone.glb\"#)"
        );
    }

    /// `find_path_referencers` lists path mentions only; `find_referencers` adds `asset:<guid>`
    /// mentions of the `.meta` id (the merge seam with agent-asset-methods' `editor.move_asset`).
    #[test]
    fn path_referencers_leave_out_id_only_references() {
        let root = temp_project("path-referencers");
        let id = source_with_meta(&root, "rock.glb", "gltf");
        std::fs::write(root.join("assets/by_path.ron"), "(mesh: \"rock.glb\")").expect("by path");
        std::fs::write(root.join("assets/by_id.ron"), format!("(mesh: \"asset:{id}\")")).expect("by id");

        assert_eq!(
            find_referencers(&root, "assets/rock.glb"),
            vec!["assets/by_id.ron".to_string(), "assets/by_path.ron".to_string()]
        );
        assert_eq!(
            find_path_referencers(&root, "assets/rock.glb"),
            vec!["assets/by_path.ron".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
