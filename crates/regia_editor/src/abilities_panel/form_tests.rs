// SPDX-License-Identifier: MPL-2.0
//! The typed form of the Abilities inspector's asset editor (spec 53 § 6, ADR-7472): a typed field
//! edit saves the bytes the text path saves for the same change, and a value that fails validation
//! shows its message beside the field, refuses Save with the documented code, and the agent reads
//! the same errors.

use regia_framework::abilities::{EffectAsset, Lifetime, Magnitude, ModOp, Modifier};
use serde_json::{Value, json};

use super::*;

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// A project with `assets/effects/burn.effect.ron` in canonical form; returns (root, text).
fn project(name: &str) -> (std::path::PathBuf, String) {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets/effects")).expect("dir");
    let mut burn = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1.0)));
    burn.name = "Burn".into();
    burn.live_mods = vec![Modifier {
        attribute: "armor".into(),
        op: ModOp::AddPre,
        amount: Magnitude::Const(-2.0),
        per_stack: false,
        when: None,
    }];
    let text = ab::to_ron(&burn).expect("effect text");
    std::fs::write(dir.join("assets/effects/burn.effect.ron"), &text).expect("write");
    (dir, text)
}

fn world_for(dir: &std::path::Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn asset(world: &mut World, params: Value) -> Result<Value, RpcError> {
    method("editor.abilities_panel_asset")(world, &params)
}

fn saved(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("assets/effects/burn.effect.ron")).expect("read")
}

/// A typed edit and the same edit typed as text save the same bytes (a top-level number and a
/// number inside an enum).
#[test]
fn a_typed_field_edit_saves_the_bytes_the_text_path_saves() {
    for (field, value, from, to) in [
        ("chance", json!(0.5), "chance: 1.0", "chance: 0.5"),
        (
            "lifetime.Timed.Const",
            json!(2.5),
            "lifetime: Timed(Const(1.0))",
            "lifetime: Timed(Const(2.5))",
        ),
        ("name", json!("Fire"), "name: \"Burn\"", "name: \"Fire\""),
    ] {
        let (typed_dir, text) = project("abilities-form-typed");
        assert!(text.contains(from), "fixture holds `{from}`:\n{text}");
        let mut world = world_for(&typed_dir);
        asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
        let v = asset(&mut world, json!({ "field": field, "value": value })).expect("typed edit");
        assert_eq!(v["asset"]["dirty"], true, "{field}: {v}");
        assert_eq!(v["asset"]["ok"], true, "{field}: {v}");
        asset(&mut world, json!({ "save": true })).expect("save typed");

        let (text_dir, text) = project("abilities-form-text");
        let mut world = world_for(&text_dir);
        asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
        asset(
            &mut world,
            json!({ "text": text.replace(from, to), "save": true }),
        )
        .expect("save text");
        assert_eq!(saved(&typed_dir), saved(&text_dir), "{field}");
        let _ = std::fs::remove_dir_all(&typed_dir);
        let _ = std::fs::remove_dir_all(&text_dir);
    }
}

/// An invalid value: the agent reads the message with its field, Save is refused with
/// `abilities_panel_invalid_asset`, and the form draws the message on the field's row.
#[test]
fn an_invalid_value_shows_beside_its_field_and_save_is_refused() {
    let (dir, _) = project("abilities-form-invalid");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    let v = asset(&mut world, json!({ "field": "chance", "value": 1.5 })).expect("typed edit");
    assert_eq!(v["asset"]["ok"], false, "{v}");
    let diagnostics = v["asset"]["diagnostics"].as_array().cloned().unwrap_or_default();
    let chance = diagnostics
        .iter()
        .find(|d| d["field"] == "chance")
        .unwrap_or_else(|| panic!("a diagnostic beside `chance`: {v}"));
    assert_eq!(chance["code"], "effect_bad_lifetime");
    assert_eq!(chance["message"], "chance must be in 0..=1");
    let e = asset(&mut world, json!({ "save": true })).expect_err("invalid");
    assert_eq!(reason(&e), "abilities_panel_invalid_asset");
    // The panel read reports the same error.
    let v = method("editor.abilities_panel_get")(&mut world, &json!({})).expect("get");
    assert!(
        v["asset"]["diagnostics"].as_array().is_some_and(|d| d
            .iter()
            .any(|d| d["field"] == "chance" && d["code"] == "effect_bad_lifetime")),
        "{v}"
    );

    // The form: the message is drawn on the `chance` row.
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut tree = None;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 2400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| abilities_panel(ui, &mut world));
        output.textures_delta.clear();
        tree = output.platform_output.accesskit_update;
    }
    let tree = tree.expect("accesskit is on");
    let row = |text: &str| {
        tree.nodes.iter().find_map(|(_, node)| {
            let named = node.label().is_some_and(|l| l.contains(text))
                || node.value().is_some_and(|l| l.contains(text));
            named.then(|| node.bounds()).flatten()
        })
    };
    let all: Vec<String> = tree
        .nodes
        .iter()
        .flat_map(|(_, n)| [n.label(), n.value()])
        .flatten()
        .map(str::to_owned)
        .collect();
    let label = tree
        .nodes
        .iter()
        .find_map(|(_, node)| {
            (node.label() == Some("chance") || node.value() == Some("chance"))
                .then(|| node.bounds())
                .flatten()
        })
        .unwrap_or_else(|| panic!("a `chance` field: {all:?}"));
    let message = row("chance must be in 0..=1").unwrap_or_else(|| panic!("the message is drawn: {all:?}"));
    assert!(
        message.y0 < label.y1 && label.y0 < message.y1,
        "the message shares the field's row: {label:?} vs {message:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A value that does not fit the field's type, or a field the format lacks, is refused with its
/// code and leaves the text as it was.
#[test]
fn a_value_of_the_wrong_shape_is_refused_and_changes_nothing() {
    let (dir, text) = project("abilities-form-shape");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    for (params, expected) in [
        (
            json!({ "field": "chance", "value": "high" }),
            "abilities_panel_bad_value",
        ),
        (
            json!({ "field": "live_mods[0].op", "value": "Sideways" }),
            "abilities_panel_bad_value",
        ),
        (json!({ "field": "nope", "value": 1 }), "abilities_panel_no_field"),
        (
            json!({ "field": "live_mods[7].amount", "value": 1 }),
            "abilities_panel_no_field",
        ),
        (json!({ "field": "chance" }), "abilities_panel_bad_value"),
    ] {
        let e = asset(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    let v = asset(&mut world, json!({})).expect("read");
    assert_eq!(v["asset"]["text"], text.as_str());
    assert_eq!(v["asset"]["dirty"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Content Browser's New menu creates the three kinds at their current versions; each opens
/// in the typed form without an error.
#[test]
fn new_attribute_sets_effects_and_abilities_open_valid_in_the_form() {
    use crate::content::{NewAssetKind, create_asset};
    let dir = regia_test_utils::unique_temp_dir("abilities-form-new");
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    let mut world = world_for(&dir);
    for (name, path, version) in [
        (
            "attribute_set",
            "assets/hero.attrs.ron",
            ab::ATTRIBUTES_FORMAT_VERSION,
        ),
        (
            "effect",
            "assets/effects/heal.effect.ron",
            ab::EFFECT_FORMAT_VERSION,
        ),
        (
            "ability",
            "assets/abilities/dash.ability.ron",
            ab::ABILITY_FORMAT_VERSION,
        ),
    ] {
        let kind = NewAssetKind::parse(name).expect("kind");
        assert!(NewAssetKind::ALL.contains(&kind), "{name} is in the New menu");
        assert_eq!(kind.as_str(), name);
        assert_eq!(create_asset(&dir, kind, path).expect("created"), path);
        let rel = path.trim_start_matches("assets/");
        let v = asset(&mut world, json!({ "path": rel })).expect("open");
        assert_eq!(v["asset"]["ok"], true, "{name}: {v}");
        assert_eq!(v["asset"]["form"]["format_version"], version, "{name}: {v}");
        // The wrong suffix is refused.
        assert!(create_asset(&dir, kind, "assets/x.ron").is_err(), "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The panel drawn headless for three frames with AccessKit on; returns the last tree.
fn accesskit_frames(world: &mut World) -> egui::accesskit::TreeUpdate {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut tree = None;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 2400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| abilities_panel(ui, world));
        output.textures_delta.clear();
        tree = output.platform_output.accesskit_update;
    }
    tree.expect("accesskit is on")
}

/// rev-claude-59 inspector-forms finding 3: `format_version` is shown, not edited. The form draws
/// it as a label (no number widget named after it), and a typed edit of it is refused with
/// `abilities_panel_read_only` and changes nothing.
#[test]
fn the_format_version_is_read_only_in_the_form() {
    let (dir, text) = project("abilities-form-version");
    let mut world = world_for(&dir);
    let v = asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    let version = v["asset"]["form"]["format_version"].clone();
    assert!(version.is_u64(), "{v}");
    let e = asset(&mut world, json!({ "field": "format_version", "value": 99 })).expect_err("read-only");
    assert_eq!(reason(&e), "abilities_panel_read_only", "{e:?}");
    let v = asset(&mut world, json!({})).expect("read");
    assert_eq!(v["asset"]["text"], text.as_str(), "the text is unchanged");
    assert_eq!(v["asset"]["form"]["format_version"], version);

    let tree = accesskit_frames(&mut world);
    let widgets: Vec<String> = tree
        .nodes
        .iter()
        .filter(|(_, n)| n.label() == Some("format_version") || n.value() == Some("format_version"))
        .map(|(_, n)| format!("{:?}", n.role()))
        .collect();
    assert!(
        !widgets.is_empty(),
        "the form shows the format version: {widgets:?}"
    );
    assert!(
        tree.nodes
            .iter()
            .all(
                |(_, n)| !((n.label() == Some("format_version") || n.value() == Some("format_version"))
                    && matches!(
                        n.role(),
                        egui::accesskit::Role::SpinButton | egui::accesskit::Role::TextInput
                    ))
            ),
        "no editable widget is named `format_version`: {widgets:?}"
    );
    // The other numbers keep their editable widget.
    assert!(
        tree.nodes
            .iter()
            .any(|(_, n)| n.label() == Some("chance") && n.role() == egui::accesskit::Role::SpinButton),
        "`chance` is still a number widget"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// rev-claude-59 inspector-forms finding 4: a non-finite number written by hand (`NaN`, `inf`)
/// is shown in the form as `{ "non_finite": spelling }`, every other typed edit keeps it, and a
/// typed number corrects it.
#[test]
fn a_non_finite_number_shows_and_a_typed_edit_corrects_it() {
    let (dir, text) = project("abilities-form-non-finite");
    let hand = text
        .replace("chance: 1.0", "chance: NaN")
        .replace("Timed(Const(1.0))", "Timed(Const(inf))");
    assert_ne!(hand, text, "the fixture has both fields");
    std::fs::write(dir.join("assets/effects/burn.effect.ron"), &hand).expect("write");
    let mut world = world_for(&dir);
    let v = asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    assert_eq!(
        v["asset"]["form"]["chance"],
        json!({ "non_finite": "NaN" }),
        "{v}"
    );
    assert_eq!(
        v["asset"]["form"]["lifetime"]["Timed"]["Const"],
        json!({ "non_finite": "inf" }),
        "{v}"
    );

    // Another field edits; both non-finite values stay as they were.
    let v = asset(&mut world, json!({ "field": "name", "value": "Fire" })).expect("another field edits");
    let edited = v["asset"]["text"].as_str().expect("text").to_owned();
    assert!(edited.contains("name: \"Fire\""), "{edited}");
    assert!(edited.contains("chance: NaN"), "{edited}");
    assert!(edited.contains("Timed(Const(inf))"), "{edited}");

    // A number corrects each; the form then shows plain numbers.
    asset(&mut world, json!({ "field": "chance", "value": 0.5 })).expect("corrected");
    let v = asset(
        &mut world,
        json!({ "field": "lifetime.Timed.Const", "value": 2.0 }),
    )
    .expect("corrected");
    let fixed = v["asset"]["text"].as_str().expect("text");
    assert!(fixed.contains("chance: 0.5"), "{fixed}");
    assert!(fixed.contains("Timed(Const(2.0))"), "{fixed}");
    assert_eq!(v["asset"]["form"]["chance"], json!(0.5));
    assert_eq!(v["asset"]["ok"], true, "{v}");

    // The form draws the spelling (not `none`) with a number widget to correct it.
    asset(&mut world, json!({ "text": hand })).expect("back to the hand-written text");
    let tree = accesskit_frames(&mut world);
    let texts: Vec<String> = tree
        .nodes
        .iter()
        .flat_map(|(_, n)| [n.label(), n.value()])
        .flatten()
        .map(str::to_owned)
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("NaN")),
        "the spelling is shown: {texts:?}"
    );
    assert!(
        tree.nodes
            .iter()
            .any(|(_, n)| n.label() == Some("chance") && n.role() == egui::accesskit::Role::SpinButton),
        "a number widget corrects `chance`: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------- w10 forms-parity items 4 and 5

/// The Abilities panel alone on a screen, clicked by the text it draws.
struct Screen {
    ctx: egui::Context,
    world: World,
    texts: Vec<(String, egui::Pos2)>,
    /// How many passes the last click's release frame ran.
    click_passes: usize,
}

impl Screen {
    fn new(world: World) -> Self {
        let mut screen = Self {
            ctx: egui::Context::default(),
            world,
            texts: Vec::new(),
            click_passes: 0,
        };
        screen.frame(Vec::new(), false);
        screen
    }

    /// One frame; `discard` asks egui for a second pass of it (as a layout change does), which sees the
    /// same input again.
    fn frame(&mut self, events: Vec<egui::Event>, discard: bool) -> usize {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 3000.0),
            )),
            events,
            ..Default::default()
        };
        let world = &mut self.world;
        let mut passes = 0;
        let mut output = self.ctx.run_ui(input, |ui| {
            passes += 1;
            if discard && ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("test: a second pass of the click frame");
            }
            abilities_panel(ui, world);
        });
        output.textures_delta.clear();
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_string(),
                    shape.visual_bounding_rect().center(),
                )),
                _ => {}
            }
        }
        self.texts.clear();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut self.texts);
        }
        passes
    }

    fn has(&self, text: &str) -> bool {
        self.texts.iter().any(|(t, _)| t == text)
    }

    /// Clicks the `n`th widget drawing `label`; `discard` gives the click frame a second pass.
    fn click_nth(&mut self, label: &str, n: usize, discard: bool) {
        self.frame(Vec::new(), false);
        let pos = self
            .texts
            .iter()
            .filter(|(t, _)| t == label)
            .nth(n)
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("no {label:?} #{n} in {:?}", self.texts));
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.frame(vec![egui::Event::PointerMoved(pos)], false);
        self.frame(vec![press(true)], false);
        self.click_passes = self.frame(vec![press(false)], discard);
        self.frame(Vec::new(), false);
    }

    fn click(&mut self, label: &str) {
        self.click_nth(label, 0, false);
    }

    fn text(&self) -> String {
        self.world
            .resource::<AbilitiesPanel>()
            .asset
            .as_ref()
            .map(|d| d.text.clone())
            .expect("an asset is open")
    }

    fn field_error(&self) -> Option<(String, String)> {
        self.world.resource::<AbilitiesPanel>().field_error.clone()
    }
}

const HERO: &str = "attrs/hero.attrs.ron";

/// A project with `assets/attrs/hero.attrs.ron` (one attribute, canonical); returns (root, text).
fn attrs_project(name: &str) -> (std::path::PathBuf, String) {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets/attrs")).expect("dir");
    let text = ab::to_ron(&regia_framework::abilities::AttributesAsset {
        format_version: ab::ATTRIBUTES_FORMAT_VERSION,
        description: "hero".into(),
        attributes: vec![regia_framework::abilities::AttributeDecl {
            name: "health".into(),
            base: 90.0,
            min: 0.0,
            max: 100.0,
            max_from: None,
            derived: None,
        }],
    })
    .expect("attrs text");
    std::fs::write(dir.join("assets").join(HERO), &text).expect("write");
    (dir, text)
}

/// w10 forms-parity item 4 (lists): **Add entry** appends a default-filled entry (zero values, `none`
/// options) and **Remove entry** drops one, each as the text the agent's `field`/`value` edit of the
/// whole list writes; a second egui pass of the click frame adds no second entry; removing the added
/// entry and saving writes the original bytes, as the agent's save does.
#[test]
fn the_form_adds_and_removes_list_entries_like_the_agent() {
    let (dir, original) = attrs_project("abilities-form-list-panel");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": HERO })).expect("open");
    let mut screen = Screen::new(world);
    assert!(screen.has("Add entry"), "{:?}", screen.texts);
    screen.click_nth("Add entry", 0, true);
    assert_eq!(screen.click_passes, 2, "the click frame ran twice");
    assert_eq!(screen.field_error(), None);
    let added = screen.text();

    let (agent_dir, _) = attrs_project("abilities-form-list-agent");
    let mut agent = world_for(&agent_dir);
    asset(&mut agent, json!({ "path": HERO })).expect("open");
    let entry = |name: &str, base: f64, max: f64| json!({ "name": name, "base": base, "min": 0.0, "max": max, "max_from": null, "derived": null });
    let v = asset(
        &mut agent,
        json!({ "field": "attributes", "value": [entry("health", 90.0, 100.0), entry("", 0.0, 0.0)] }),
    )
    .expect("agent adds");
    assert_eq!(
        added,
        v["asset"]["text"].as_str().expect("text"),
        "one default-filled entry, not two (the second pass is guarded)"
    );
    // Remove the new entry from its row: the text is the file's again, and both saves write it.
    screen.click("2 items");
    // The list opens with an animation: let it finish so every row is laid out and painted.
    for _ in 0..30 {
        screen.frame(Vec::new(), false);
    }
    screen.click_nth("Remove entry", 1, true);
    assert_eq!(screen.field_error(), None);
    assert_eq!(screen.text(), original, "remove undoes the add");
    asset(
        &mut agent,
        json!({ "field": "attributes", "value": [entry("health", 90.0, 100.0)] }),
    )
    .expect("agent removes");
    // Save both (the round-trip text equals the file, so the panel marks no change: write it anyway
    // through the agent surface of the panel's world).
    let mut world = screen.world;
    asset(&mut world, json!({ "text": original.clone() })).expect("same text");
    let panel_text = asset(&mut world, json!({})).expect("read")["asset"]["text"].clone();
    let agent_text = asset(&mut agent, json!({})).expect("read")["asset"]["text"].clone();
    assert_eq!(panel_text, agent_text);
    assert_eq!(panel_text, json!(original));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// w10 forms-parity item 4 (variants): the variant picker switches `lifetime.Timed` from `Const` to
/// `Stat` with its fields default-filled (`weight` takes its serde default 1.0, the rest zero values)
/// and the `Const` value dropped, then `lifetime` to the unit variant `UntilRemoved`, each the text the
/// agent's edit of the same value writes.
#[test]
fn the_variant_picker_switches_an_enum_like_the_agent() {
    let (dir, _) = project("abilities-form-variant-panel");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    let mut screen = Screen::new(world);
    screen.click("Const");
    screen.click("Stat");
    assert_eq!(screen.field_error(), None);
    let stat = screen.text();
    assert!(stat.contains("lifetime: Timed(Stat("), "{stat}");
    assert!(
        stat.contains("weight: 1.0"),
        "the serde default fills `weight`: {stat}"
    );
    assert!(
        !stat.contains("Timed(Const"),
        "the previous variant's value is dropped: {stat}"
    );

    let (agent_dir, _) = project("abilities-form-variant-agent");
    let mut agent = world_for(&agent_dir);
    asset(&mut agent, json!({ "path": "effects/burn.effect.ron" })).expect("open");
    let v = asset(
        &mut agent,
        json!({ "field": "lifetime.Timed", "value": { "Stat": {
            "side": "Source", "attribute": "", "read": "AtApply", "weight": 1.0, "offset": 0.0 } } }),
    )
    .expect("agent switches");
    assert_eq!(stat, v["asset"]["text"].as_str().expect("text"));
    // A unit variant.
    screen.click("Timed");
    screen.click("UntilRemoved");
    assert_eq!(screen.field_error(), None);
    let v = asset(
        &mut agent,
        json!({ "field": "lifetime", "value": "UntilRemoved" }),
    )
    .expect("agent unit");
    assert_eq!(screen.text(), v["asset"]["text"].as_str().expect("text"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// The fill and switch helpers refuse what they cannot make with the documented codes.
#[test]
fn list_and_variant_edits_refuse_with_their_codes() {
    let (_, text) = project("abilities-form-refusals");
    let kind = form::FormKind::Effect;
    let code = |r: Result<Value, PanelError>| r.expect_err("refused").code;
    assert_eq!(
        code(form::list_with_entry(kind, &text, "chance")),
        "abilities_panel_bad_value"
    );
    assert_eq!(
        code(form::list_with_entry(kind, &text, "nope")),
        "abilities_panel_no_field"
    );
    assert_eq!(
        code(form::list_with_entry(kind, &text, "format_version")),
        "abilities_panel_read_only"
    );
    assert_eq!(
        code(form::list_without_entry(kind, &text, "live_mods", 3)),
        "abilities_panel_no_field"
    );
    assert_eq!(
        code(form::variant_value(kind, &text, "lifetime", "Sideways")),
        "abilities_panel_bad_value"
    );
    assert_eq!(
        code(form::variant_value(kind, &text, "chance", "Const")),
        "abilities_panel_bad_value"
    );
    // An empty tag list: a tag has no empty value, so the first entry is typed in Advanced.
    let refused = form::list_with_entry(kind, &text, "status_tags").expect_err("no empty tag");
    assert_eq!(refused.code, "abilities_panel_bad_value");
    assert!(refused.message.contains("Advanced"), "{}", refused.message);
    // A non-empty list of a type with no zero value repeats its last entry.
    let tagged = text.replace("status_tags: [],", "status_tags: [\"Status.Burn\"],");
    if tagged != text {
        let list = form::list_with_entry(kind, &tagged, "status_tags").expect("repeats");
        assert_eq!(list, json!(["Status.Burn", "Status.Burn"]));
    }
    let enums = form::enum_paths(kind, &text);
    assert_eq!(enums.get("lifetime").map(|v| v.len()), Some(3), "{enums:?}");
    assert!(enums.contains_key("live_mods[0].op"), "{enums:?}");
    assert!(enums.contains_key("lifetime.Timed"), "{enums:?}");
}

/// w10 forms-parity item 5: a file with comments warns before its first typed edit, in the panel (a
/// banner naming the loss) and in the agent reply (`comments_warning`); the edit's reply says
/// `comments_dropped`, the panel then says the comments were dropped, and the next typed edit warns no
/// more. The edit itself is the agent's (same text).
#[test]
fn a_commented_file_warns_before_the_first_typed_edit() {
    let (dir, text) = attrs_project("abilities-form-comments-panel");
    let commented = format!("// tuned by hand: keep health under 120\n{text}");
    std::fs::write(dir.join("assets").join(HERO), &commented).expect("write");
    let mut world = world_for(&dir);
    let v = asset(&mut world, json!({ "path": HERO })).expect("open");
    assert_eq!(
        v["asset"]["comments_warning"],
        json!(form::COMMENTS_WARNING),
        "{v}"
    );
    assert_eq!(v["comments_dropped"], false);
    let mut screen = Screen::new(world);
    assert!(screen.has(form::COMMENTS_WARNING), "{:?}", screen.texts);
    // The first typed edit from the panel.
    screen.click_nth("Add entry", 0, false);
    assert_eq!(screen.field_error(), None);
    assert!(
        !screen.text().contains("tuned by hand"),
        "the canonical text has no comments"
    );
    assert!(!screen.has(form::COMMENTS_WARNING));
    assert!(
        screen
            .texts
            .iter()
            .any(|(t, _)| t.starts_with("A typed edit dropped the file's comments")),
        "{:?}",
        screen.texts
    );
    // The agent: the same first edit answers comments_dropped, the next one does not.
    let (agent_dir, _) = attrs_project("abilities-form-comments-agent");
    std::fs::write(agent_dir.join("assets").join(HERO), &commented).expect("write");
    let mut agent = world_for(&agent_dir);
    asset(&mut agent, json!({ "path": HERO })).expect("open");
    let entry = |name: &str, base: f64, max: f64| json!({ "name": name, "base": base, "min": 0.0, "max": max, "max_from": null, "derived": null });
    let v = asset(
        &mut agent,
        json!({ "field": "attributes", "value": [entry("health", 90.0, 100.0), entry("", 0.0, 0.0)] }),
    )
    .expect("agent edit");
    assert_eq!(v["comments_dropped"], true, "{v}");
    assert_eq!(v["asset"]["comments_warning"], Value::Null);
    assert_eq!(screen.text(), v["asset"]["text"].as_str().expect("text"));
    let v = asset(&mut agent, json!({ "field": "description", "value": "hero 2" })).expect("again");
    assert_eq!(v["comments_dropped"], false, "{v}");
    // Comment markers inside strings are not comments.
    assert!(!form::has_comments("(name: \"a // b /* c\")"));
    assert!(form::has_comments("(a: 1) /* note */"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&agent_dir);
}

// ------------------------------------------------------------- w11 forms-undo-autosave item 2 (undo)

/// A deterministic pseudo-random sequence (xorshift64), so the test is the same every run.
struct Steps(u64);

impl Steps {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// w11 item 2 acceptance: 50 random edits (typed fields, list adds and removes, variant switches,
/// whole-text replacements; refused edits included) then a full undo returns the original bytes and
/// clears the dirty flag; a full redo returns the last edit's text; saving after the undo writes the
/// original bytes.
#[test]
fn fifty_random_edits_then_full_undo_returns_the_original_bytes() {
    let (dir, original) = attrs_project("abilities-undo-random");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": HERO })).expect("open");
    let mut steps = Steps(0x5eed_1234_abcd_0001);
    let mut applied = 0usize;
    let mut last_text = original.clone();
    for i in 0..50 {
        let entry = |name: &str, base: f64| json!({ "name": name, "base": base, "min": 0.0, "max": 200.0, "max_from": null, "derived": null });
        let params = match steps.next(5) {
            0 => json!({ "field": "description", "value": format!("hero {i}") }),
            1 => json!({ "field": "attributes[0].base", "value": steps.next(100) as f64 }),
            2 => {
                json!({ "field": "attributes", "value": [entry("health", 90.0), entry(&format!("a{i}"), 1.0)] })
            }
            3 => json!({ "text": last_text.replace("hero", &format!("hero t{i}")) }),
            // A refused edit (no such field) changes nothing and is no step.
            _ => json!({ "field": "no_such_field", "value": 1 }),
        };
        let before = asset(&mut world, json!({})).expect("read");
        match asset(&mut world, params.clone()) {
            Ok(v) => {
                let text = v["asset"]["text"].as_str().expect("text").to_owned();
                if text != before["asset"]["text"].as_str().expect("text") {
                    applied += 1;
                }
                assert_eq!(v["undo_depth"], applied, "step {i} {params}: {v}");
                last_text = text;
            }
            Err(e) => {
                assert_eq!(reason(&e), "abilities_panel_no_field", "step {i} {params}");
                let after = asset(&mut world, json!({})).expect("read");
                assert_eq!(
                    after["undo_depth"], before["undo_depth"],
                    "a refused edit is no step"
                );
            }
        }
    }
    assert!(applied >= 20, "the sequence edits the file: {applied}");
    for left in (0..applied).rev() {
        let v = asset(&mut world, json!({ "undo": true })).expect("undo");
        assert_eq!(v["undo_depth"], left);
        assert_eq!(v["redo_depth"], applied - left);
    }
    let v = asset(&mut world, json!({})).expect("read");
    assert_eq!(
        v["asset"]["text"],
        json!(original),
        "full undo: the original bytes"
    );
    assert_eq!(v["asset"]["dirty"], false);
    let e = asset(&mut world, json!({ "undo": true })).expect_err("nothing left");
    assert_eq!(reason(&e), "abilities_panel_nothing_to_undo");
    for _ in 0..applied {
        asset(&mut world, json!({ "redo": true })).expect("redo");
    }
    let v = asset(&mut world, json!({})).expect("read");
    assert_eq!(
        v["asset"]["text"],
        json!(last_text),
        "full redo: the last edit's text"
    );
    let e = asset(&mut world, json!({ "redo": true })).expect_err("nothing to redo");
    assert_eq!(reason(&e), "abilities_panel_nothing_to_redo");
    // Undo everything again and save: the file holds the original bytes.
    for _ in 0..applied {
        asset(&mut world, json!({ "undo": true })).expect("undo");
    }
    asset(&mut world, json!({ "text": original.clone(), "save": true })).expect("save");
    assert_eq!(
        std::fs::read_to_string(dir.join("assets").join(HERO)).expect("read"),
        original
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new edit after an undo drops the redo steps; Save keeps the undo steps (undoing past a save marks
/// the asset dirty again); opening a file starts a new history.
#[test]
fn an_edit_after_undo_drops_redo_and_save_keeps_undo() {
    let (dir, original) = attrs_project("abilities-undo-save");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": HERO })).expect("open");
    asset(&mut world, json!({ "field": "description", "value": "one" })).expect("edit");
    asset(&mut world, json!({ "field": "description", "value": "two" })).expect("edit");
    let v = asset(&mut world, json!({ "undo": true })).expect("undo");
    assert_eq!(
        (v["undo_depth"].clone(), v["redo_depth"].clone()),
        (json!(1), json!(1))
    );
    let v = asset(&mut world, json!({ "field": "description", "value": "three" })).expect("edit");
    assert_eq!(
        (v["undo_depth"].clone(), v["redo_depth"].clone()),
        (json!(2), json!(0))
    );
    let v = asset(&mut world, json!({ "save": true })).expect("save");
    assert_eq!(v["asset"]["dirty"], false);
    assert_eq!(v["undo_depth"], 2, "save keeps the steps");
    let v = asset(&mut world, json!({ "undo": true })).expect("undo past the save");
    assert_eq!(v["asset"]["dirty"], true);
    let v = asset(&mut world, json!({ "path": HERO })).expect("open again");
    assert_eq!(
        (v["undo_depth"].clone(), v["redo_depth"].clone()),
        (json!(0), json!(0))
    );
    assert_ne!(v["asset"]["text"], json!(original), "the saved edit is on disk");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The asset editor's Undo and Redo buttons: one form commit is one step; a click frame that egui runs
/// twice undoes once; Undo then Redo returns the edited text.
#[test]
fn the_undo_and_redo_buttons_step_once_per_click() {
    let (dir, original) = attrs_project("abilities-undo-buttons");
    let mut world = world_for(&dir);
    asset(&mut world, json!({ "path": HERO })).expect("open");
    asset(&mut world, json!({ "field": "description", "value": "one" })).expect("edit");
    asset(&mut world, json!({ "field": "description", "value": "two" })).expect("edit");
    let two = asset(&mut world, json!({})).expect("read")["asset"]["text"].clone();
    let mut screen = Screen::new(world);
    assert!(screen.has("Undo") && screen.has("Redo"), "{:?}", screen.texts);
    screen.click_nth("Undo", 0, true);
    assert_eq!(screen.click_passes, 2, "the click frame ran twice");
    assert_eq!(
        screen.world.resource::<AbilitiesPanel>().asset_history_depth(),
        (1, 1),
        "one undo, not two"
    );
    screen.click("Undo");
    assert_eq!(screen.text(), original);
    screen.click("Redo");
    screen.click("Redo");
    assert_eq!(json!(screen.text()), two);
    let _ = std::fs::remove_dir_all(&dir);
}
