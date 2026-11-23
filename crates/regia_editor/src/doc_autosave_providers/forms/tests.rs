// SPDX-License-Identifier: MPL-2.0
//! w11 forms-undo-autosave item 4: per kind, an unsaved edit autosaves (the bytes the editor's save
//! writes) and recovery in a new session opens the file in its editor with the edit over it, unsaved.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_app::prelude::App;

use super::*;
use crate::doc_autosave::{RecoveryOffers, TickInput, flush, install, restore, tick};

const FLUSH: Duration = Duration::from_secs(60);

fn project(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("w11-autosave-{label}"))
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

fn editor_world(root: &Path) -> App {
    let mut app = App::new();
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    install(world);
    app
}

/// One forced tick of the crashed session writes exactly one `kind` document with `expected` bytes
/// (when given); a new session's Restore brings it back. Returns the new session.
fn crash_and_recover(root: &Path, mut first: App, kind: &str, expected: Option<&[u8]>) -> App {
    let world = first.world_mut();
    let dirty = crate::doc_autosave::dirty_documents(world);
    assert_eq!(dirty.len(), 1, "{kind}: one dirty document: {dirty:?}");
    assert_eq!(dirty[0].kind, kind);
    if let Some(expected) = expected {
        let snapshots = crate::doc_autosave::take_snapshots(world);
        assert_eq!(snapshots.len(), 1, "{kind}");
        assert_eq!(
            String::from_utf8_lossy(&snapshots[0].bytes),
            String::from_utf8_lossy(expected),
            "{kind}: the snapshot is what a save writes"
        );
    }
    let report = tick(
        world,
        TickInput {
            project_root: root,
            now: Instant::now(),
            force: true,
        },
    );
    assert_eq!(report.docs.len(), 1, "{kind}: {report:?}");
    assert_eq!(flush(world, FLUSH), 1, "{kind}");
    drop(first);

    let mut app = editor_world(root);
    let world = app.world_mut();
    let offers = RecoveryOffers::scan(root);
    assert_eq!(offers.docs.len(), 1, "{kind}: {offers:?}");
    assert_eq!(offers.docs[0].entry.kind, kind);
    world.insert_resource(offers);
    let outcome = restore(world, &[]);
    assert_eq!(outcome.done.len(), 1, "{kind}: {outcome:?}");
    assert!(outcome.failed.is_empty(), "{kind}: {outcome:?}");
    app
}

#[test]
fn the_five_kinds_are_registered() {
    let mut registry = crate::doc_autosave::DocAutosaveRegistry::default();
    crate::doc_autosave_providers::register_builtin(&mut registry);
    let kinds = registry.kinds();
    for kind in ["cloth", "ragdoll", "vehicle", "abilities", "loc"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
}

#[test]
fn cloth_recovery_restores_an_unsaved_edit() {
    use regia::physics::cloth::asset::ClothSource;
    let root = project("cloth");
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"), params: (substeps: 4))"#,
    )
    .expect("parses");
    write(
        &root,
        "cloth/flag.cloth.ron",
        &source.to_ron_string().expect("ron"),
    );
    let mut first = editor_world(&root);
    let world = first.world_mut();
    crate::cloth_panel::open_file(world, "cloth/flag.cloth.ron").expect("opens");
    let expected = {
        let mut panel = world.resource_mut::<crate::cloth_panel::ClothPanel>();
        let doc = panel.doc_mut().expect("doc");
        if let crate::cloth_panel::PanelSource::Cloth(c) = &mut doc.source {
            c.params.substeps = 9;
        }
        assert!(doc.dirty());
        doc.save_text().expect("text").into_bytes()
    };
    let mut app = crash_and_recover(&root, first, "cloth", Some(&expected));
    let panel = app.world_mut().resource::<crate::cloth_panel::ClothPanel>();
    let doc = panel.doc().expect("the file is open again");
    assert_eq!(doc.path, "cloth/flag.cloth.ron");
    assert!(doc.dirty(), "unsaved until a save");
    assert_eq!(doc.save_text().expect("text").into_bytes(), expected);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ragdoll_recovery_restores_an_unsaved_edit() {
    use regia_framework::ragdoll::RagdollProfile;
    let root = project("ragdoll");
    write(
        &root,
        "chars/hero.ragdoll.ron",
        &RagdollProfile::default().to_ron().expect("ron"),
    );
    let mut first = editor_world(&root);
    let world = first.world_mut();
    let mut panel = crate::ragdoll_panel::RagdollPanel::default();
    panel.open(Some(&root), "chars/hero.ragdoll.ron").expect("opens");
    let mut edited = panel.profile.clone().expect("profile");
    edited.forward = regia_core::math::Vec3::Z;
    panel.profile = Some(edited.clone());
    panel.dirty = true;
    world.insert_resource(panel);
    let expected = edited.to_ron().expect("ron").into_bytes();
    let mut app = crash_and_recover(&root, first, "ragdoll", Some(&expected));
    let panel = app.world_mut().resource::<crate::ragdoll_panel::RagdollPanel>();
    assert_eq!(panel.path.as_deref(), Some("chars/hero.ragdoll.ron"));
    assert!(panel.dirty);
    assert_eq!(panel.profile.as_ref(), Some(&edited));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vehicle_recovery_restores_an_unsaved_edit() {
    use regia::physics::vehicle_asset::VehicleAsset;
    let root = project("vehicle");
    let car = VehicleAsset::new("car", Default::default());
    write(&root, "vehicles/car.vehicle.ron", &car.to_ron().expect("ron"));
    let mut first = editor_world(&root);
    let world = first.world_mut();
    let mut tuning = crate::vehicle_tuning::VehicleTuning::default();
    tuning
        .open(Some(&root), "vehicles/car.vehicle.ron", false)
        .expect("opens");
    if let Some(asset) = tuning.asset.as_mut() {
        asset.name = "fast car".into();
    }
    assert!(tuning.is_dirty());
    let expected = crate::vehicle_tuning::save_text(
        tuning.source.as_deref(),
        tuning.opened.as_ref(),
        tuning.asset.as_ref().expect("asset"),
    )
    .expect("text")
    .0
    .into_bytes();
    world.insert_resource(tuning);
    let mut app = crash_and_recover(&root, first, "vehicle", Some(&expected));
    let tuning = app.world_mut().resource::<crate::vehicle_tuning::VehicleTuning>();
    assert_eq!(tuning.path.as_deref(), Some("vehicles/car.vehicle.ron"));
    assert!(tuning.is_dirty());
    assert_eq!(tuning.asset.as_ref().map(|a| a.name.as_str()), Some("fast car"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn abilities_recovery_restores_an_unsaved_edit() {
    use regia_framework::abilities as ab;
    let root = project("abilities");
    let text = ab::to_ron(&ab::AttributesAsset {
        format_version: ab::ATTRIBUTES_FORMAT_VERSION,
        description: "hero".into(),
        attributes: Vec::new(),
    })
    .expect("text");
    write(&root, "assets/attrs/hero.attrs.ron", &text);
    let mut first = editor_world(&root);
    let world = first.world_mut();
    let mut panel = crate::abilities_panel::AbilitiesPanel::default();
    panel
        .open_asset(world, Some(&root), "attrs/hero.attrs.ron")
        .expect("opens");
    panel
        .set_asset_field(world, Some(&root), "description", &json!("edited"))
        .expect("edit");
    let expected = panel.asset.as_ref().expect("doc").text.clone();
    assert_ne!(expected, text);
    world.insert_resource(panel);
    let mut app = crash_and_recover(&root, first, "abilities", Some(expected.as_bytes()));
    let panel = app
        .world_mut()
        .resource::<crate::abilities_panel::AbilitiesPanel>();
    let doc = panel.asset.as_ref().expect("the asset is open again");
    assert_eq!(doc.path, "attrs/hero.attrs.ron");
    assert_eq!(doc.text, expected);
    assert_eq!(doc.saved, text, "unsaved until a save");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn loc_recovery_restores_an_unapplied_translation() {
    let root = project("loc");
    write(
        &root,
        "assets/localization/localization.ron",
        "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\"])\n",
    );
    write(&root, "assets/localization/en/main.ftl", "hello = Hello\n");
    write(&root, "assets/localization/fr/main.ftl", "hello = Bonjour\n");
    let mut first = editor_world(&root);
    let world = first.world_mut();
    // Read the table the way the panel does, then select the cell and type without applying.
    crate::loc_panel::with_panel(world, |_, _| Ok(())).expect("reads the files");
    {
        let mut panel = world.resource_mut::<crate::loc_panel::LocPanel>();
        panel.selected = Some(("hello".into(), "fr".into()));
        panel.edit_text = "Bonjour".into();
    }
    assert!(
        crate::doc_autosave::dirty_documents(world).is_empty(),
        "the cell's own text is no edit"
    );
    world.resource_mut::<crate::loc_panel::LocPanel>().edit_text = "Salut".into();
    let mut app = crash_and_recover(&root, first, "loc", None);
    let panel = app.world_mut().resource::<crate::loc_panel::LocPanel>();
    assert_eq!(panel.selected, Some(("hello".into(), "fr".into())));
    assert_eq!(panel.edit_text, "Salut");
    // Nothing was written: the file still holds the applied text.
    assert_eq!(
        std::fs::read_to_string(root.join("assets/localization/fr/main.ftl")).expect("read"),
        "hello = Bonjour\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}
