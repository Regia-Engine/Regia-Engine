// SPDX-License-Identifier: MPL-2.0
//! Asset package tests: the dependency closure, export with `.meta` sidecars and the manifest, import
//! into another project, and the refusals (path clash, id clash, damaged file) that copy nothing.

use super::*;

/// Review sprint-w12-rev-g5 #11: a package job whose worker never starts fails (`pkg_io`) instead
/// of staying Running, with Cancel disabled, for the session.
#[test]
fn a_package_job_that_never_starts_is_failed() {
    let mut registry = regia_agent::jobs::JobRegistry::default();
    let (_id, handle) = registry.start("package", "export", false);
    let unstarted = handle.clone();
    // The worker closure (and its handle) dropped by a failed spawn.
    drop(handle);
    let status = spawn_failed(&unstarted, &std::io::Error::other("no threads left"));
    assert!(status.contains("no threads left"), "{status}");
    assert_eq!(unstarted.state(), regia_agent::jobs::JobState::Failed);
}

/// Review sprint-w12-rev-g5 #10: a package or thumbnail click acts once per egui frame, so the
/// second UI pass neither opens the folder dialog again nor queues a second import.
#[test]
fn a_menu_click_acts_once_per_frame() {
    let mut world = World::new();
    assert!(first_click_in_frame(&mut world, 8));
    assert!(!first_click_in_frame(&mut world, 8), "the second pass of frame 8");
    assert!(first_click_in_frame(&mut world, 9));
    assert_eq!(world.resource::<MenuClickFrame>().0, Some(9));
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, bytes).expect("write");
}

fn write_meta(root: &Path, rel: &str) -> String {
    let id = regia_asset::AssetId::new_random();
    let meta = regia_asset::MetaSidecar::new("test", id);
    let text = meta.to_ron_string().expect("meta");
    std::fs::write(regia_asset::meta_path_for(&root.join(rel)), text).expect("meta file");
    id.to_string()
}

/// A project whose prefab names a mesh by path and a material by id; the material names a texture by
/// path; a `.gltf` names its buffer; one asset nobody references.
fn source_project(tag: &str) -> (PathBuf, BTreeMap<&'static str, String>) {
    let root = regia_test_utils::unique_temp_dir(tag);
    let mut ids = BTreeMap::new();
    write(
        &root,
        "assets/textures/bark.png",
        &crate::os_drop::tests::tiny_png(),
    );
    ids.insert("bark", write_meta(&root, "assets/textures/bark.png"));
    write(
        &root,
        "assets/materials/bark.material.ron",
        b"(base_color_texture: \"textures/bark.png\")",
    );
    ids.insert("mat", write_meta(&root, "assets/materials/bark.material.ron"));
    write(
        &root,
        "assets/props/crate.glb",
        &crate::os_drop::tests::glb_triangle(),
    );
    ids.insert("crate", write_meta(&root, "assets/props/crate.glb"));
    write(&root, "assets/props/tree.bin", &[0u8; 12]);
    write(
        &root,
        "assets/props/tree.gltf",
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"tree.bin","byteLength":12}],"images":[{"uri":"data:image/png;base64,AA=="}]}"#,
    );
    let prefab = format!(
        "(mesh: \"assets/props/crate.glb\", material: \"asset:{}\", extra: \"props/tree.gltf\")",
        ids["mat"]
    );
    write(&root, "assets/props/crate.prefab.ron", prefab.as_bytes());
    write(&root, "assets/unrelated.png", &crate::os_drop::tests::tiny_png());
    // A longer path that contains a packaged one must not count as a reference to it.
    write(&root, "assets/notes.ron", b"(path: \"old/props/crate.glb.bak\")");
    (root, ids)
}

#[test]
fn the_closure_follows_paths_ids_and_gltf_uris_only() {
    let (root, _) = source_project("pkg_closure");
    let index = ReferenceIndex::build(&root);
    let closure = dependency_closure(&index, &["assets/props/crate.prefab.ron".to_string()]);
    assert_eq!(
        closure,
        [
            "assets/materials/bark.material.ron",
            "assets/props/crate.glb",
            "assets/props/crate.prefab.ron",
            "assets/props/tree.bin",
            "assets/props/tree.gltf",
            "assets/textures/bark.png",
        ]
    );
    assert!(
        index.deps["assets/notes.ron"].is_empty(),
        "{:?}",
        index.deps["assets/notes.ron"]
    );
}

#[test]
fn export_then_import_keeps_paths_and_ids_and_refuses_clashes() {
    let (source, ids) = source_project("pkg_source");
    let out = regia_test_utils::unique_temp_dir("pkg_out");
    let dest = out.join("crate-package");
    let report =
        export_package(&source, &["assets/props/crate.prefab.ron".to_string()], &dest).expect("export");
    assert_eq!(report.manifest.assets.len(), 6);
    assert!(dest.join(ASSET_PACKAGE_MANIFEST).is_file());
    assert!(regia_asset::meta_path_for(&dest.join("assets/props/crate.glb")).is_file());
    assert!(!dest.join("assets/unrelated.png").exists());
    let manifest = read_manifest(&dest).expect("manifest");
    assert_eq!(manifest.roots, ["assets/props/crate.prefab.ron"]);
    // A second export into the same folder is refused.
    let again =
        export_package(&source, &["assets/props/crate.glb".to_string()], &dest).expect_err("not empty");
    assert_eq!(again.code, "pkg_dest_not_empty");

    // Another project imports it: same paths, same ids.
    let target = regia_test_utils::unique_temp_dir("pkg_target");
    std::fs::create_dir_all(target.join("assets")).expect("assets");
    let imported = import_package(&target, &dest, false).expect("import");
    assert!(
        imported.clashes.is_empty() && imported.damaged.is_empty(),
        "{imported:?}"
    );
    assert_eq!(imported.imported.len(), 6);
    assert_eq!(
        asset_id(&target, "assets/props/crate.glb").as_deref(),
        Some(ids["crate"].as_str())
    );
    assert_eq!(
        asset_id(&target, "assets/materials/bark.material.ron").as_deref(),
        Some(ids["mat"].as_str())
    );

    // Importing it again clashes on every path; nothing changes.
    let twice = import_package(&target, &dest, false).expect("refusal report");
    assert_eq!(twice.clashes.iter().filter(|c| c.kind == "path").count(), 6);
    assert!(twice.imported.is_empty());
    assert!(
        twice.summary().contains("nothing was imported"),
        "{}",
        twice.summary()
    );

    // A project that uses one of the ids at another path refuses the whole package.
    let other = regia_test_utils::unique_temp_dir("pkg_idclash");
    write(
        &other,
        "assets/mine/box.glb",
        &crate::os_drop::tests::glb_triangle(),
    );
    let meta = regia_asset::MetaSidecar::new(
        "test",
        regia_asset::MetaSidecar::from_ron(
            &std::fs::read(regia_asset::meta_path_for(&source.join("assets/props/crate.glb"))).expect("read"),
        )
        .expect("meta")
        .id,
    );
    std::fs::write(
        regia_asset::meta_path_for(&other.join("assets/mine/box.glb")),
        meta.to_ron_string().expect("ron"),
    )
    .expect("meta");
    let refused = import_package(&other, &dest, false).expect("refusal report");
    assert_eq!(refused.clashes.len(), 1, "{refused:?}");
    assert_eq!(refused.clashes[0].kind, "id");
    assert_eq!(
        refused.clashes[0].existing.as_deref(),
        Some("assets/mine/box.glb")
    );
    assert!(!other.join("assets/props").exists(), "nothing was copied");

    // A damaged file is refused too.
    std::fs::write(dest.join("assets/props/tree.bin"), [1u8; 12]).expect("damage");
    let fresh = regia_test_utils::unique_temp_dir("pkg_damaged");
    let damaged = import_package(&fresh, &dest, false).expect("refusal report");
    assert_eq!(damaged.damaged, ["assets/props/tree.bin"]);
    assert!(!fresh.join("assets/props").exists());
}

/// P-145: the Content Browser selection's Export selection with dependencies queues one package of
/// the whole selection; running it writes one manifest whose roots are the selected assets and
/// whose files are the union of their dependency closures (each file once).
#[test]
fn bulk_export_packs_the_selection_with_dependencies() {
    let (source, _) = source_project("pkg_bulk");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(source.clone()),
    });
    let dest = regia_test_utils::unique_temp_dir("pkg_bulk_out").join("selection");
    let selection = [
        "assets/props/crate.prefab.ron".to_string(),
        "assets/unrelated.png".to_string(),
        // Selected twice (a range plus a Ctrl-click): one root.
        "assets/props/crate.prefab.ron".to_string(),
    ];
    request_export(&mut world, &selection, dest.clone());
    let Some(PackageRequest::Export(roots, to)) = world.resource::<PendingPackageRequest>().0.clone() else {
        panic!("one export request");
    };
    assert_eq!(roots, ["assets/props/crate.prefab.ron", "assets/unrelated.png"]);
    assert_eq!(to, dest);
    // The queued request is what run_pending hands the package job.
    let report = export_package(&source, &roots, &dest).expect("export");
    let manifest = read_manifest(&dest).expect("one manifest");
    assert_eq!(
        manifest.roots,
        ["assets/props/crate.prefab.ron", "assets/unrelated.png"]
    );
    let paths: Vec<&str> = manifest.assets.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "assets/materials/bark.material.ron",
            "assets/props/crate.glb",
            "assets/props/crate.prefab.ron",
            "assets/props/tree.bin",
            "assets/props/tree.gltf",
            "assets/textures/bark.png",
            "assets/unrelated.png",
        ],
        "the selection and every dependency, once each"
    );
    assert_eq!(report.manifest.assets.len(), 7);
    assert!(dest.join("assets/unrelated.png").is_file());
    assert!(regia_asset::meta_path_for(&dest.join("assets/props/crate.glb")).is_file());
    let _ = std::fs::remove_dir_all(&source);
    let _ = std::fs::remove_dir_all(dest.parent().unwrap_or(&dest));
}

#[test]
fn export_refuses_bad_roots_and_a_destination_inside_the_project() {
    let (source, _) = source_project("pkg_refusals");
    let dest = regia_test_utils::unique_temp_dir("pkg_refusals_out").join("p");
    for (roots, code) in [
        (vec![], "pkg_bad_params"),
        (vec!["../secret.ron".to_string()], "pkg_bad_path"),
        (vec!["assets/none.ron".to_string()], "pkg_missing"),
    ] {
        assert_eq!(
            export_package(&source, &roots, &dest)
                .map(|_| ())
                .map_err(|e| e.code),
            Err(code)
        );
    }
    let inside = source.join("export");
    assert_eq!(
        export_package(&source, &["assets/unrelated.png".to_string()], &inside)
            .map(|_| ())
            .map_err(|e| e.code),
        Err("pkg_bad_dest")
    );
    let missing = regia_test_utils::unique_temp_dir("pkg_nomanifest");
    assert_eq!(
        read_manifest(&missing).map(|_| ()).map_err(|e| e.code),
        Err("pkg_no_manifest")
    );
}

#[test]
fn the_agent_methods_export_and_import_through_the_extension() {
    let (source, _) = source_project("pkg_agent");
    let mut world = World::new();
    crate::agent_ext::install(&mut world);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(source.clone()),
    });
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, export) = extension.method("editor.export_package").expect("registered");
    let (_, import) = extension.method("editor.import_package").expect("registered");
    let code = |err: RpcError| err.data.and_then(|d| d.get("pkg_error").cloned());
    let err = export(&mut world, &json!({ "paths": [], "dest": "x" })).expect_err("empty");
    assert_eq!(code(err), Some(json!("pkg_bad_params")));
    let dest = regia_test_utils::unique_temp_dir("pkg_agent_out").join("pkg");
    let reply = export(
        &mut world,
        &json!({ "paths": ["assets/textures/bark.png"], "dest": dest.display().to_string() }),
    )
    .expect("export");
    assert_eq!(reply["assets"].as_array().map(Vec::len), Some(1));
    let target = regia_test_utils::unique_temp_dir("pkg_agent_target");
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(target.clone()),
    });
    let reply = import(
        &mut world,
        &json!({ "package": dest.display().to_string(), "cook": false }),
    )
    .expect("import");
    assert_eq!(reply["refused"], json!(false));
    assert_eq!(reply["imported"], json!(["assets/textures/bark.png"]));
    let reply = import(
        &mut world,
        &json!({ "package": dest.display().to_string(), "cook": false }),
    )
    .expect("again");
    assert_eq!(reply["refused"], json!(true));
    assert_eq!(reply["clashes"][0]["kind"], json!("path"));
}
