// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixtures for `.ui.ron`: the checked-in v1 example (the `docs/formats/` example as
//! it was before ADR-1381, copied into `fixtures/`) must keep loading and upgrade to the current
//! version, the v2 fixture (menu widgets), the v3 fixture (view-model bindings, ADR-1811) and the
//! v4 fixture (text effects, ADR-2045) upgrade by renumbering only, and the v5 fixture
//! (`backdrop_blur`, ADR-2129) loads as is (`docs/specs/12-assets.md` § 4a). Round trips: the
//! text upgrade (`upgrade_ui_ron`) parsed back to the same tree, and the canonical writer
//! (`write_ui_ron`, used by the UI designer) parsed back to an equal tree.

use std::sync::Arc;

use regia_ui::authoring::{BindTargetDef, WidgetDef};
use regia_ui::{
    BindMode, UI_RON_VERSION, UiMigrators, parse_ui_ron, ui_schema_fingerprint, upgrade_ui_ron, write_ui_ron,
};

/// The checked-in `.ui.ron` v1 fixture still loads, asserting the fixture's own content (not a
/// test-local migrator).
#[test]
fn ui_v1_fixture_still_loads() {
    let file = parse_ui_ron(
        include_str!("fixtures/ui.example.ui.ron"),
        "ui.example.ui.ron",
        &UiMigrators::default(),
    )
    .expect("v1 ui");
    assert_eq!(file.version, UI_RON_VERSION);
    assert_eq!(file.root.children.len(), 2);
    // The fixture's actual nodes: a bound score label and a button with a localized span.
    let score = &file.root.children[0];
    assert_eq!(score.name.as_deref(), Some("score"));
    assert_eq!(score.bind.as_ref().expect("bind").source, "Score.value");
    let button = &file.root.children[1];
    assert!(matches!(button.widget, Some(WidgetDef::Button)), "fixture button");
}

/// A newer `.ui.ron` version is rejected with a clear error.
#[test]
fn ui_rejects_newer_version() {
    let newer = include_str!("fixtures/ui.example.ui.ron").replace("version: 1", "version: 99");
    let err =
        parse_ui_ron(&newer, "ui.example.ui.ron", &UiMigrators::default()).expect_err("v99 must be rejected");
    assert!(err.to_string().contains("version 99"), "{err}");
}

/// An older `.ui.ron` version is upgraded through a registered migrator.
#[test]
fn ui_migrates_an_older_version() {
    let mut migrators = UiMigrators::default();
    migrators.register(
        0,
        "test",
        Arc::new(|text| {
            *text = text.replacen("version: 0", "version: 1", 1);
            Ok(1)
        }),
    );
    let older = include_str!("fixtures/ui.example.ui.ron").replace("version: 1", "version: 0");
    let file = parse_ui_ron(&older, "ui.example.ui.ron", &migrators).expect("migrated v0 -> v1");
    assert_eq!(file.version, UI_RON_VERSION);
}

/// The ui schema fingerprint hashes *nested* field names (not just the root struct's).
#[test]
fn ui_schema_fingerprint_hashes_nested_fields() {
    let fp = ui_schema_fingerprint();
    assert!(fp.contains("version"), "{fp}");
    assert!(fp.contains("root"), "{fp}");
    // `widget` lives on the nested `UiNodeDef`, and `font_px` on the nested `TextStyle`.
    assert!(fp.contains("widget"), "nested UiNodeDef not hashed: {fp}");
    assert!(fp.contains("font_px"), "nested TextStyle not hashed: {fp}");
}

/// The v1 fixture upgrades to the current version by renumbering only, and the upgraded text loads
/// as the same tree (round trip); upgrading the result again changes nothing.
#[test]
fn ui_v1_fixture_upgrades_and_round_trips() {
    let v1 = include_str!("fixtures/ui.example.ui.ron");
    let v6 = upgrade_ui_ron(v1, "ui.example.ui.ron").expect("upgrade v1");
    assert_eq!(v6, v1.replacen("version: 1", "version: 11", 1));
    let from_v1 = parse_ui_ron(v1, "v1", &UiMigrators::default()).expect("v1 loads");
    let from_v6 = parse_ui_ron(&v6, "v6", &UiMigrators::default()).expect("v6 loads");
    assert_eq!(from_v6.version, UI_RON_VERSION);
    assert_eq!(from_v1, from_v6);
    assert_eq!(upgrade_ui_ron(&v6, "v6").expect("current"), v6);
}

/// The checked-in v2 fixture (menu bar, popup menu, context menu) upgrades to v6 by renumbering
/// only and keeps its menus, and the published example in `docs/formats/` is at the current
/// version.
#[test]
fn ui_v2_fixture_loads_menus() {
    let src = include_str!("fixtures/ui.v2.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v2").expect("upgrade v2"),
        src.replacen("version: 2", "version: 11", 1),
        "v2 -> v3 -> v4 -> v5 -> v6 -> v7 -> v8 -> v9 -> v10 renumbers only"
    );
    let file = parse_ui_ron(src, "ui.v2.example.ui.ron", &UiMigrators::default()).expect("v2 ui");
    assert_eq!(file.version, 11);
    let Some(WidgetDef::MenuBar(menus)) = &file.root.children[0].widget else {
        panic!("menu bar");
    };
    assert_eq!(menus.len(), 2);
    assert_eq!(menus[0].submenu.len(), 3);
    assert!(menus[0].submenu[1].separator);
    assert!(matches!(
        file.root.children[1].widget,
        Some(WidgetDef::PopupMenu(_))
    ));
    assert!(matches!(
        file.root.children[2].widget,
        Some(WidgetDef::ContextMenu(_))
    ));

    let docs = include_str!("../../../docs/formats/ui-ron.example.ui.ron");
    let example = parse_ui_ron(docs, "ui-ron.example.ui.ron", &UiMigrators::default()).expect("docs example");
    assert_eq!(
        upgrade_ui_ron(docs, "docs").expect("current"),
        docs,
        "docs example is current"
    );
    assert_eq!(example.version, UI_RON_VERSION);
}

/// The checked-in v5 fixture (`backdrop_blur`) upgrades to v11 by renumbering only, keeps the field
/// on the tree, and writes back unchanged; a node without it stays without.
#[test]
fn ui_v5_fixture_loads_backdrop_blur() {
    let src = include_str!("fixtures/ui.v5.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v5").expect("upgrade v5"),
        src.replacen("version: 5", "version: 11", 1),
        "v5 -> v6 -> v7 -> v8 -> v9 -> v10 renumbers only"
    );
    let file = parse_ui_ron(src, "ui.v5.example.ui.ron", &UiMigrators::default()).expect("v5 ui");
    assert_eq!(file.version, 11);
    assert_eq!(file.root.backdrop_blur, None);
    assert_eq!(file.root.children[0].backdrop_blur, Some(12.0));
    assert_eq!(file.root.children[1].backdrop_blur, None);
    let written = write_ui_ron(&file).expect("writes");
    assert_eq!(
        parse_ui_ron(&written, "written", &UiMigrators::default()).expect("back"),
        file
    );
    assert!(written.contains("backdrop_blur: 12"), "{written}");
    assert_eq!(
        written.matches("backdrop_blur").count(),
        1,
        "absent stays absent: {written}"
    );
}

/// The checked-in v10 fixture (the game widgets, ADR-2958) upgrades to v11 by renumbering only
/// (ADR-9980), keeps every field, and reads back from the writer's text unchanged.
#[test]
fn ui_v10_fixture_loads_game_widgets() {
    let src = include_str!("fixtures/ui.v10.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v10").expect("upgrade v10"),
        src.replacen("version: 10", "version: 11", 1),
        "only the version number changes"
    );
    let file = parse_ui_ron(src, "ui.v10.example.ui.ron", &UiMigrators::default()).expect("v10 ui");
    assert_eq!(file.version, UI_RON_VERSION);
    let Some(WidgetDef::LobbyList(list)) = &file.root.children[0].widget else {
        panic!("lobby list");
    };
    assert_eq!(list.title, "Matches");
    assert_eq!(list.launch, "loc:lobby-start");
    let Some(WidgetDef::Scoreboard(board)) = &file.root.children[1].widget else {
        panic!("scoreboard");
    };
    assert_eq!(board.hold_action, "Scores");
    assert_eq!(board.columns[0].key, "tags");
    assert_eq!(board.columns[0].width, 64.0);
    assert!(!board.group_by_team);
    let Some(WidgetDef::Radar(radar)) = &file.root.children[2].widget else {
        panic!("radar");
    };
    assert_eq!(radar.range, 75.0);
    assert!(radar.rotate);
    let Some(WidgetDef::Radar(map)) = &file.root.children[3].widget else {
        panic!("minimap");
    };
    assert_eq!(map.world_rect, Some([-50.0, -50.0, 50.0, 50.0]));
    assert_eq!(map.map_image, 77);
    let Some(WidgetDef::MarkerLayer(layer)) = &file.root.children[4].widget else {
        panic!("marker layer");
    };
    assert_eq!(layer.edge_margin, 40.0);
    let written = write_ui_ron(&file).expect("writes");
    assert_eq!(
        parse_ui_ron(&written, "written", &UiMigrators::default()).expect("back"),
        file,
        "{written}"
    );
}

/// The checked-in v11 fixture (ADR-9980) loads without migration: the old two-element text input
/// reads with no options, the options element and `fit` keep every field, and the writer's text
/// reads back to an equal tree.
#[test]
fn ui_v11_fixture_loads_text_input_options_and_image_fit() {
    use regia_ui::ImageFit;
    use regia_ui::authoring::{TextFilterDef, TextInputOptions};
    let src = include_str!("fixtures/ui.v11.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v11").expect("current"),
        src,
        "nothing to migrate"
    );
    let file = parse_ui_ron(src, "ui.v11.example.ui.ron", &UiMigrators::default()).expect("v11 ui");
    assert_eq!(file.version, UI_RON_VERSION);
    let kids = &file.root.children;
    let options = |i: usize| match &kids[i].widget {
        Some(WidgetDef::TextInput(_, _, options)) => options.clone(),
        other => panic!("child {i}: {other:?}"),
    };
    assert!(options(0).is_default());
    assert_eq!(
        options(1),
        TextInputOptions {
            max_chars: 4,
            secret: Some('*'),
            read_only: false,
            filter: TextFilterDef::Integer,
        }
    );
    assert!(options(2).read_only);
    let fit = |i: usize| kids[i].image.as_ref().expect("image").effective_fit();
    assert_eq!(
        [fit(3), fit(4), fit(5)],
        [ImageFit::Cover, ImageFit::Stretch, ImageFit::Contain]
    );
    let written = write_ui_ron(&file).expect("writes");
    assert!(written.contains("text_input(\"Name\", false)"), "{written}");
    assert_eq!(
        parse_ui_ron(&written, "written", &UiMigrators::default()).expect("back"),
        file,
        "{written}"
    );
}

/// The checked-in v3 fixture (view-model bindings, two-way mode, invert, commands) upgrades by
/// renumbering only and keeps every field.
#[test]
fn ui_v3_fixture_loads_bindings_and_commands() {
    let src = include_str!("fixtures/ui.v3.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v3").expect("upgrade v3"),
        src.replacen("version: 3", "version: 11", 1),
        "v3 -> v9 -> v10 renumbers only"
    );
    let file = parse_ui_ron(src, "ui.v3.example.ui.ron", &UiMigrators::default()).expect("v3 ui");
    assert_eq!(file.version, 11);
    let hp = &file.root.children[0];
    assert_eq!(hp.bindings.len(), 2);
    assert_eq!(hp.bindings[0].source, "HudVm.health");
    assert_eq!(hp.bindings[0].target, BindTargetDef::Progress);
    assert_eq!(hp.bindings[1].target, BindTargetDef::Visible);
    assert!(hp.bindings[1].invert);
    let volume = &file.root.children[1];
    assert_eq!(volume.bindings[0].mode, BindMode::TwoWay);
    assert_eq!(volume.bindings[0].target, BindTargetDef::Value);
    let apply = &file.root.children[2];
    assert_eq!(apply.command.as_deref(), Some("settings.apply"));
    assert_eq!(apply.bindings[0].mode, BindMode::OneTime);
    assert_eq!(apply.bindings[0].target, BindTargetDef::Enabled);
    let name = &file.root.children[3];
    assert_eq!(name.bindings[0].target, BindTargetDef::Input);
    assert_eq!(name.command.as_deref(), Some("profile.rename"));
    // A file newer than this loader is refused.
    let newer = src.replacen("version: 3", &format!("version: {}", UI_RON_VERSION + 1), 1);
    assert!(parse_ui_ron(&newer, "newer", &UiMigrators::default()).is_err());
}

/// The checked-in v4 fixture (ADR-2045 text styles) upgrades to v6 by renumbering only and keeps
/// every v4 field; a style without them writes no new keys.
#[test]
fn ui_v4_fixture_loads_text_effects() {
    let src = include_str!("fixtures/ui.v4.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v4").expect("upgrade v4"),
        src.replacen("version: 4", "version: 11", 1),
        "v4 -> v5 -> v6 -> v7 -> v8 -> v9 -> v10 renumbers only"
    );
    let file = parse_ui_ron(src, "ui.v4.example.ui.ron", &UiMigrators::default()).expect("v4 ui");
    assert_eq!(file.version, 11);
    let title = file.root.children[0].text.as_ref().expect("title text");
    assert!((title.style.letter_spacing - 0.1).abs() < 1e-6);
    assert_eq!(title.style.weight, Some(700));
    assert_eq!(title.style.effects.len(), 2);
    assert_eq!(title.style.effects[0].width, 2.0);
    assert_eq!(title.style.effects[1].offset, [3.0, 3.0]);
    assert_eq!(title.style.effects[1].blur, 4.0);
    let subtitle = file.root.children[1].text.as_ref().expect("subtitle text");
    assert_eq!(
        subtitle.style.features,
        vec!["smcp".to_owned(), "liga=0".to_owned()]
    );
    let written = write_ui_ron(&file).expect("write");
    assert!(
        written.contains("letter_spacing") && written.contains("effects"),
        "{written}"
    );
    let plain = include_str!("fixtures/ui.v3.example.ui.ron");
    let plain =
        write_ui_ron(&parse_ui_ron(plain, "v3", &UiMigrators::default()).expect("v3")).expect("write");
    for key in [
        "letter_spacing",
        "weight",
        "underline",
        "strikethrough",
        "features",
        "effects",
    ] {
        assert!(!plain.contains(key), "unset `{key}` written:\n{plain}");
    }
}

/// The checked-in v6 fixture (ADR-2489) upgrades to v7 by renumbering only (ADR-2866), keeps
/// `repeat` and every converter, and writes back to an equal tree; a file without them writes no
/// new keys.
#[test]
fn ui_v6_fixture_loads_repeat_and_converters() {
    use regia_ui::BindConverter;
    let src = include_str!("fixtures/ui.v6.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v6").expect("upgrade v6"),
        src.replacen("version: 6", "version: 11", 1),
        "v6 -> v7 -> v8 -> v9 -> v10 renumbers only"
    );
    let file = parse_ui_ron(src, "ui.v6.example.ui.ron", &UiMigrators::default()).expect("v6 ui");
    assert_eq!(file.version, 11);
    let rows = &file.root.children[0];
    let repeat = rows.repeat.as_ref().expect("repeat");
    assert_eq!(repeat.source, "InventoryVm.items");
    assert_eq!(repeat.key.as_deref(), Some("id"));
    let row = &rows.children[0];
    let converters: Vec<&'static str> = row
        .children
        .iter()
        .filter_map(|c| c.bindings.first())
        .filter_map(|b| b.converter.as_ref())
        .map(BindConverter::name)
        .collect();
    assert_eq!(
        converters,
        [
            "number",
            "map_range",
            "bool_to_visibility",
            "loc_key",
            "enum_text",
            "clamp"
        ]
    );
    let written = write_ui_ron(&file).expect("write");
    assert_eq!(
        parse_ui_ron(&written, "written", &UiMigrators::default()).expect("back"),
        file
    );
    let plain = include_str!("fixtures/ui.v5.example.ui.ron");
    let plain =
        write_ui_ron(&parse_ui_ron(plain, "v5", &UiMigrators::default()).expect("v5")).expect("write");
    for key in ["repeat", "converter"] {
        assert!(!plain.contains(key), "unset `{key}` written:\n{plain}");
    }
}

/// A designer save merges into every fixture and the docs example without falling back on the
/// canonical form, and an edit leaves all the other lines alone (ADR-2489).
#[test]
fn every_fixture_merges_an_edit_and_keeps_its_other_lines() {
    use regia_ui::ron_merge::merge_ui_ron;
    for (name, src) in [
        ("v1", include_str!("fixtures/ui.example.ui.ron")),
        ("v2", include_str!("fixtures/ui.v2.example.ui.ron")),
        ("v3", include_str!("fixtures/ui.v3.example.ui.ron")),
        ("v4", include_str!("fixtures/ui.v4.example.ui.ron")),
        ("v5", include_str!("fixtures/ui.v5.example.ui.ron")),
        ("v6", include_str!("fixtures/ui.v6.example.ui.ron")),
        ("v7", include_str!("fixtures/ui.v7.example.ui.ron")),
        ("v9", include_str!("fixtures/ui.v9.example.ui.ron")),
        (
            "docs",
            include_str!("../../../docs/formats/ui-ron.example.ui.ron"),
        ),
    ] {
        let source = upgrade_ui_ron(src, name).expect("upgrade");
        let old = parse_ui_ron(&source, name, &UiMigrators::default()).expect("parse");
        // One property: a new command on the first child.
        let mut new = old.clone();
        new.root.children[0].command = Some("merged.command".to_owned());
        let text = merge_ui_ron(&source, &old, &new).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            parse_ui_ron(&text, name, &UiMigrators::default()).expect("merged"),
            new,
            "{name}"
        );
        for line in source.lines() {
            assert!(text.lines().any(|l| l == line), "{name}: lost {line:?}\n{text}");
        }
        // Structure: the first child removed, the rest kept, the last one moved first.
        let mut new = old.clone();
        new.root.children.remove(0);
        if let Some(last) = new.root.children.pop() {
            new.root.children.insert(0, last);
        }
        let text = merge_ui_ron(&source, &old, &new).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            parse_ui_ron(&text, name, &UiMigrators::default()).expect("merged"),
            new,
            "{name}"
        );
    }
}

/// The canonical writer round-trips every fixture and the docs example: written text parses back
/// to an equal tree, writing again gives the same bytes, and the text is at the current version.
#[test]
fn write_ui_ron_round_trips_every_fixture() {
    let sources = [
        include_str!("fixtures/ui.example.ui.ron"),
        include_str!("fixtures/ui.v2.example.ui.ron"),
        include_str!("fixtures/ui.v3.example.ui.ron"),
        include_str!("fixtures/ui.v4.example.ui.ron"),
        include_str!("fixtures/ui.v5.example.ui.ron"),
        include_str!("fixtures/ui.v9.example.ui.ron"),
        include_str!("../../../docs/formats/ui-ron.example.ui.ron"),
    ];
    for (i, src) in sources.iter().enumerate() {
        let file = parse_ui_ron(src, "fixture", &UiMigrators::default()).expect("fixture parses");
        let text = write_ui_ron(&file).expect("write");
        let back = parse_ui_ron(&text, "written", &UiMigrators::default())
            .unwrap_or_else(|e| panic!("fixture {i}: written text does not parse: {e}\n{text}"));
        assert_eq!(back, file, "fixture {i} round trip\n{text}");
        assert_eq!(
            write_ui_ron(&back).expect("write again"),
            text,
            "fixture {i}: stable bytes"
        );
        assert!(text.contains("version: 11"), "{text}");
    }
}

/// The checked-in v7 fixture (ADR-2866) loads the group fields, spawns their components, and
/// writes back to an equal tree; a v6 file gains nothing but the number.
#[test]
fn ui_v7_fixture_loads_group_opacity_clips_and_masks() {
    use regia_ui::authoring::{ClipShapeDef, MaskDef};
    let src = include_str!("fixtures/ui.v7.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v7").expect("upgrade v7"),
        src.replacen("version: 7", "version: 11", 1),
        "v7 -> v8 -> v9 -> v10 renumbers only (ADR-3147, ADR-3148, ADR-2958)"
    );
    let file = parse_ui_ron(src, "ui.v7.example.ui.ron", &UiMigrators::default()).expect("v7 ui");
    assert_eq!(file.version, 11);
    let inventory = &file.root.children[0];
    assert_eq!(inventory.group_opacity, Some(0.5));
    assert_eq!(inventory.clip_shape, Some(ClipShapeDef::Rounded));
    assert_eq!(
        file.root.children[1].mask,
        Some(MaskDef {
            image: Some(7),
            asset: None,
            icon: None,
            fit: regia_ui::MaskFit::Contain
        })
    );
    assert_eq!(file.root.children[2].clip_shape, Some(ClipShapeDef::Rect));
    assert_eq!(file.root.group_opacity, None);
    let written = write_ui_ron(&file).expect("writes");
    assert!(written.contains("group_opacity: 0.5"), "{written}");
    assert!(written.contains("clip_shape: rounded"), "{written}");
    assert!(written.contains("fit: contain"), "{written}");
    let back = parse_ui_ron(&written, "written", &UiMigrators::default()).expect("re-parse");
    assert_eq!(back, file);
    // Stretch is the default fit and is not written.
    let mut stretch = file.clone();
    stretch.root.children[1].mask = Some(MaskDef {
        image: Some(7),
        asset: None,
        icon: None,
        fit: regia_ui::MaskFit::Stretch,
    });
    let text = write_ui_ron(&stretch).expect("writes");
    assert!(
        text.contains("mask: (image: 7)") || !text.contains("fit: stretch"),
        "{text}"
    );
    // A v6 file has none of the fields.
    let v6 = parse_ui_ron(
        include_str!("fixtures/ui.v6.example.ui.ron"),
        "v6",
        &UiMigrators::default(),
    )
    .expect("v6");
    assert!(
        v6.root
            .children
            .iter()
            .all(|c| c.group_opacity.is_none() && c.mask.is_none())
    );
}

/// A mask names an image by id or by manifest path, exactly one of the two.
#[test]
fn ui_v7_mask_names_one_image() {
    use regia_ui::authoring::MaskDef;
    let doc =
        |mask: &str| format!("(version: 7, root: (name: \"root\", children: [(name: \"m\", mask: {mask})]))");
    let file = parse_ui_ron(
        &doc("(asset: \"assets/ui/star.png\", fit: tile)"),
        "asset",
        &UiMigrators::default(),
    )
    .expect("by asset");
    assert_eq!(
        file.root.children[0].mask,
        Some(MaskDef {
            image: None,
            asset: Some("assets/ui/star.png".to_owned()),
            icon: None,
            fit: regia_ui::MaskFit::Tile
        })
    );
    let back = parse_ui_ron(
        &write_ui_ron(&file).expect("writes"),
        "written",
        &UiMigrators::default(),
    )
    .expect("re-parse");
    assert_eq!(back, file);
    for bad in ["(fit: tile)", "(image: 3, asset: \"assets/ui/star.png\")"] {
        let err = parse_ui_ron(&doc(bad), "bad", &UiMigrators::default()).expect_err(bad);
        assert!(matches!(err, regia_ui::UiError::Invalid { .. }), "{bad}: {err}");
        assert!(err.to_string().contains("\"m\""), "names the node: {err}");
    }
}

/// The checked-in v9 fixture (ADR-3148) loads images and masks named by asset path and icon with
/// nine-slice insets, writes back to an equal tree with stable bytes, lists its picture references
/// and cook dependencies, and spawns the named-picture components (v9 -> v11 renumbers only); the v7 fixture migrates to v11
/// by renumbering only and keeps its raw-id mask.
#[test]
fn ui_v9_fixture_names_pictures_by_asset_and_icon() {
    use regia_ui::authoring::{ImageDef, MaskDef};
    let src = include_str!("fixtures/ui.v9.example.ui.ron");
    assert_eq!(
        upgrade_ui_ron(src, "v9").expect("upgrade v9"),
        src.replacen("version: 9", "version: 11", 1),
        "v9 -> v10 renumbers only (ADR-2958)"
    );
    let file = parse_ui_ron(src, "ui.v9.example.ui.ron", &UiMigrators::default()).expect("v9 ui");
    assert_eq!(file.version, 11);
    let kids = &file.root.children;
    assert_eq!(
        kids[0].image.as_ref().and_then(|i| i.asset.as_deref()),
        Some("assets/ui/logo.png")
    );
    assert_eq!(
        kids[1].image.as_ref().and_then(|i| i.icon.as_deref()),
        Some("gear")
    );
    assert_eq!(kids[1].image.as_ref().map(|i| i.id), Some(0));
    assert_eq!(
        kids[2].image.as_ref().and_then(|i| i.nine_slice),
        Some((12.0, 12.0, 12.0, 12.0))
    );
    assert_eq!(
        kids[3].mask,
        Some(MaskDef {
            image: None,
            asset: None,
            icon: Some("ring".to_owned()),
            fit: regia_ui::MaskFit::Contain
        })
    );
    assert_eq!(kids[3].children[0].image, Some(ImageDef::raw(7)));
    let text = write_ui_ron(&file).expect("write");
    assert!(text.contains("asset: \"assets/ui/logo.png\""), "{text}");
    assert!(text.contains("icon: \"gear\""), "{text}");
    assert!(!text.contains("id: 0"), "a named picture writes no id: {text}");
    let back = parse_ui_ron(&text, "written", &UiMigrators::default()).expect("re-parse");
    assert_eq!(back, file);
    assert_eq!(write_ui_ron(&back).expect("again"), text, "stable bytes");

    let refs: Vec<(String, &str, &str, String)> = regia_ui::ui_image_references(&file)
        .into_iter()
        .map(|r| (r.node, r.field, r.kind, r.value))
        .collect();
    assert_eq!(
        refs,
        vec![
            ("/0".to_owned(), "image", "asset", "assets/ui/logo.png".to_owned()),
            ("/1".to_owned(), "image", "icon", "gear".to_owned()),
            (
                "/2".to_owned(),
                "image",
                "asset",
                "assets/ui/panel.png".to_owned()
            ),
            ("/3".to_owned(), "mask", "icon", "ring".to_owned()),
            ("/3/0".to_owned(), "image", "id", "7".to_owned()),
            ("/4".to_owned(), "mask", "asset", "assets/ui/star.png".to_owned()),
        ]
    );
    assert_eq!(
        regia_ui::ui_asset_dependencies(&file),
        ["assets/ui/logo.png", "assets/ui/panel.png", "assets/ui/star.png"]
    );

    // An old file migrates and round-trips; its raw-id mask stays a raw id.
    let v7 = include_str!("fixtures/ui.v7.example.ui.ron");
    let upgraded = upgrade_ui_ron(v7, "v7").expect("upgrade");
    assert_eq!(upgraded, v7.replacen("version: 7", "version: 11", 1));
    let old = parse_ui_ron(&upgraded, "v7", &UiMigrators::default()).expect("v7 as v9");
    assert_eq!(old.root.children[1].mask.as_ref().and_then(|m| m.image), Some(7));
    assert!(regia_ui::ui_asset_dependencies(&old).is_empty());
}

/// A picture named twice, an empty name, a mask with two names and a negative inset are refused
/// when the file loads (ADR-3148); a nine-slice raw id loads.
#[test]
fn ui_v9_refuses_ambiguous_pictures() {
    let doc = |node: &str| format!("(version: 9, root: (children: [{node}]))");
    for bad in [
        r#"(image: (id: 3, asset: "a.png"))"#,
        r#"(image: (asset: "a.png", icon: "x"))"#,
        r#"(image: (asset: " "))"#,
        r#"(image: (icon: ""))"#,
        r#"(image: (asset: "a.png", nine_slice: (1, -1, 1, 1)))"#,
        r#"(mask: (icon: "x", asset: "a.png"))"#,
        r#"(mask: (icon: ""))"#,
        r#"(mask: (fit: contain))"#,
    ] {
        let err = parse_ui_ron(&doc(bad), "bad", &UiMigrators::default()).expect_err(bad);
        assert!(matches!(err, regia_ui::UiError::Invalid { .. }), "{bad}: {err}");
    }
    let ok = parse_ui_ron(
        &doc("(image: (id: 3, nine_slice: (2, 2, 2, 2)))"),
        "ok",
        &UiMigrators::default(),
    )
    .expect("raw id with insets");
    assert_eq!(ok.root.children[0].image.as_ref().map(|i| i.id), Some(3));
}
