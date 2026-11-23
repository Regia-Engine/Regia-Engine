// SPDX-License-Identifier: MPL-2.0
//! Save all fixtures (see `super::Fixture`).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::Fixture;

/// Writes `text` to `rel` under `root` (making its folders); returns the absolute path.
fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, text).expect("write");
    path
}

/// A `.cloth.ron` open in the cloth panel with its substeps changed.
fn cloth(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use regia::physics::cloth::asset::ClothSource;
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"), params: (substeps: 4))"#,
    )
    .expect("parses");
    let file = write(
        root,
        "cloth/flag.cloth.ron",
        &source.to_ron_string().expect("ron"),
    );
    let world = app.world_mut();
    crate::cloth_panel::open_file(world, "cloth/flag.cloth.ron").expect("opens");
    let mut panel = world.resource_mut::<crate::cloth_panel::ClothPanel>();
    let doc = panel.doc_mut().expect("doc");
    if let crate::cloth_panel::PanelSource::Cloth(c) = &mut doc.source {
        c.params.substeps = 9;
    }
    assert!(doc.dirty(), "cloth: the edit makes the doc dirty");
    file
}

/// A `.ragdoll.ron` open in the Ragdoll editor with its forward axis changed.
fn ragdoll(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use regia_framework::ragdoll::RagdollProfile;
    let file = write(
        root,
        "chars/hero.ragdoll.ron",
        &RagdollProfile::default().to_ron().expect("ron"),
    );
    let mut panel = crate::ragdoll_panel::RagdollPanel::default();
    panel.open(Some(root), "chars/hero.ragdoll.ron").expect("opens");
    if let Some(profile) = panel.profile.as_mut() {
        profile.forward = regia_core::math::Vec3::Z;
    }
    panel.dirty = true;
    panel.generation += 1;
    app.world_mut().insert_resource(panel);
    file
}

/// A `.vehicle.ron` open in Vehicle Tuning with its name changed.
fn vehicle(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use regia::physics::vehicle_asset::VehicleAsset;
    let car = VehicleAsset::new("car", Default::default());
    let file = write(root, "vehicles/car.vehicle.ron", &car.to_ron().expect("ron"));
    let mut tuning = crate::vehicle_tuning::VehicleTuning::default();
    tuning
        .open(Some(root), "vehicles/car.vehicle.ron", false)
        .expect("opens");
    if let Some(asset) = tuning.asset.as_mut() {
        asset.name = "fast car".into();
    }
    assert!(tuning.is_dirty(), "vehicle: the edit makes the tuning dirty");
    app.world_mut().insert_resource(tuning);
    file
}

/// An `.attrs.ron` open in the Abilities asset editor with its description changed (valid text).
fn abilities(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use regia_framework::abilities as ab;
    let text = ab::to_ron(&ab::AttributesAsset {
        format_version: ab::ATTRIBUTES_FORMAT_VERSION,
        description: "hero".into(),
        attributes: Vec::new(),
    })
    .expect("text");
    let file = write(root, "assets/attrs/hero.attrs.ron", &text);
    let world = app.world_mut();
    let mut panel = crate::abilities_panel::AbilitiesPanel::default();
    panel
        .open_asset(world, Some(root), "attrs/hero.attrs.ron")
        .expect("opens");
    panel
        .set_asset_field(world, Some(root), "description", &json!("edited"))
        .expect("edit");
    assert_ne!(
        panel.asset.as_ref().map(|doc| doc.text.as_str()),
        Some(text.as_str()),
        "abilities: the edit changes the text"
    );
    world.insert_resource(panel);
    file
}

/// The Localization panel's edit box holding unapplied text for `hello` in `fr`; Save all applies
/// it to the string table `assets/localization/fr/main.ftl`.
fn loc(app: &mut regia_app::App, root: &Path) -> PathBuf {
    write(
        root,
        "assets/localization/localization.ron",
        "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\"])\n",
    );
    write(root, "assets/localization/en/main.ftl", "hello = Hello\n");
    let file = write(root, "assets/localization/fr/main.ftl", "hello = Bonjour\n");
    let world = app.world_mut();
    // Read the table the way the panel does, then select the cell and type without applying.
    crate::loc_panel::with_panel(world, |_, _| Ok(())).expect("reads the files");
    let mut panel = world.resource_mut::<crate::loc_panel::LocPanel>();
    panel.selected = Some(("hello".into(), "fr".into()));
    panel.edit_text = "Salut".into();
    file
}

/// A camera asset that loads valid (`player`), as the camera panel's own tests use.
const CAMERA: &str = r#"(format_version: 2, name: "player",
    params: { "len": Float(4.0) },
    variables: [(name: "aiming", default: Bool(false))],
    inputs: [(name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0))],
    rigs: [
        (name: "walk", root: [
            Boom(length: Param("len")),
            Group(enabled: Literal(true), nodes: [Fov(degrees: Literal(70.0))]),
        ], transitions: [(from: Rig("walk"), to: Rig("aim"), blend: Smooth(0.25))]),
        (name: "aim", root: [Fov(degrees: Literal(40.0))]),
        (name: "spare", root: [Fov(degrees: Literal(50.0))]),
    ],
    transitions: [(from: Any, to: Any, blend: Linear(0.5))],
    director: Rules(default: "walk", rules: [(when: VarBool("aiming", true), rig: "aim")]))"#;

/// A `.camera.ron` loaded on the Cameras panel with a rig added through `editor.camera_panel_edit`.
fn camera(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let file = write(root, "assets/cams/p.camera.ron", CAMERA);
    let method = |name: &str| {
        crate::camera_panel::methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .map(|(_, handler)| handler)
            .expect("registered")
    };
    let world = app.world_mut();
    method("editor.camera_panel_load")(world, &json!({ "path": "cams/p.camera.ron" })).expect("load");
    method("editor.camera_panel_edit")(world, &json!({ "op": "add_rig", "name": "crouch" })).expect("edit");
    file
}

/// A standard material edited inline in the Inspector (ADR-8107): metallic changes in the
/// document and the file changes only when Save all runs.
fn inline_asset(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = crate::content::create_asset(
        root,
        crate::content::NewAssetKind::Material,
        "assets/materials/inline_save.material.ron",
    )
    .expect("material");
    let edited =
        crate::inspector::inline_asset::set_field(app.world_mut(), &rel, "base.metallic", json!(0.4), false)
            .expect("inline edit");
    assert!(edited.dirty, "inline_asset: the edit is unsaved");
    root.join(&rel)
}

/// A `.smartobject.ron` open in the definition editor with a slot added (one undo step).
fn smart_object(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::smart_object_def_panel::{SmartObjectDefOp, default_slot, edit_def, open_def};
    let mut def = regia_ai::smart_object_def::SmartObjectDef::new();
    def.slots.push(default_slot("seat"));
    let rel = "assets/props/bench.smartobject.ron";
    let file = write(root, rel, &def.to_ron().expect("serialize"));
    let world = app.world_mut();
    open_def(world, rel).expect("open");
    let changed = edit_def(world, &SmartObjectDefOp::AddSlot { name: "stand".into() }).expect("edit");
    assert!(changed, "smart_object: the edit changes the definition");
    file
}

/// A `.motiondb.ron` open in the motion database editor with its skeleton path changed
/// (integ-v06m's provider, saved by the Save all hook).
fn motion_db(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = "assets/motion/locomotion.motiondb.ron";
    let doc = crate::mm_db_editor::new_document("assets/chars/hero.skeleton.ron");
    let text = crate::mm_db_editor::canonical_text(&doc).expect("ron");
    let file = write(root, rel, &text);
    crate::mm_db_editor::open(app.world_mut(), &file, true).expect("open");
    let changed = crate::mm_db_editor::edit(
        app.world_mut(),
        "skeleton",
        None,
        &[crate::mm_db_editor::ops::MmDbOp::SetSkeleton {
            path: "assets/chars/other.skeleton.ron".into(),
        }],
    )
    .expect("edit");
    assert!(changed, "motion_db: the edit changes the database");
    assert!(
        app.world()
            .resource::<crate::mm_db_editor::MmDbEditor>()
            .is_dirty()
    );
    file
}

/// The fixtures of this file.
pub(super) fn fixtures() -> Vec<(&'static str, Fixture)> {
    vec![
        ("cloth", cloth as Fixture),
        ("ragdoll", ragdoll as Fixture),
        ("vehicle", vehicle as Fixture),
        ("abilities", abilities as Fixture),
        ("loc", loc as Fixture),
        ("camera", camera as Fixture),
        ("smart_object", smart_object as Fixture),
        ("inline_asset", inline_asset as Fixture),
        ("motion_db", motion_db as Fixture),
    ]
}
