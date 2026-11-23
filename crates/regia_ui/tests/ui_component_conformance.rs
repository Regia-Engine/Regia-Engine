// SPDX-License-Identifier: MPL-2.0
//! Spec 111 conformance (`docs/specs/111-ui-components.md`, ADR-3147): the parts of the format the
//! other component tests do not reach. Every overridable property of § 2.1 has its documented
//! effect, an instance's `name`, `classes` and `style` apply to the component root (§ 2.2), and the
//! `MAX_COMPONENT_INSTANCES` guard of § 4 step 1 stops at exactly 4,096 instances with
//! `ui_component_too_large` placeholders, nesting included. Pure expansion: no app, no window.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use regia_ui::authoring::{BackgroundDef, UiNodeDef, WidgetDef};
use regia_ui::ui_component::{
    MAX_COMPONENT_INSTANCES, PLACEHOLDER_CLASS, PLACEHOLDER_NAME, expand, text_loader,
};
use regia_ui::{Dimension, Display};
use regia_ui::{UiColor, UiMigrators, parse_ui_ron};

fn files(list: Vec<(String, String)>) -> impl FnMut(&str) -> Result<String, String> {
    let map: HashMap<String, String> = list.into_iter().collect();
    move |p: &str| map.get(p).cloned().ok_or_else(|| format!("{p}: not found"))
}

fn doc(text: &str) -> regia_ui::authoring::UiFile {
    parse_ui_ron(text, "doc", &UiMigrators::default()).expect("parses")
}

fn child<'a>(node: &'a UiNodeDef, name: &str) -> &'a UiNodeDef {
    node.children
        .iter()
        .find(|c| c.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no child {name}"))
}

fn literal(node: &UiNodeDef) -> String {
    let text = node.text.as_ref().expect("text");
    assert_eq!(text.spans.len(), 1, "one literal span");
    match &text.spans[0] {
        regia_ui::authoring::TextSpanDef::Lit(s) => s.clone(),
        other => panic!("not a literal span: {other:?}"),
    }
}

/// One parameter per property of the § 2.1 table, each with a target node.
const ALL_PROPS: &str = r##"(
    version: 9,
    component: (
        params: [
            (name: "text", type: string, default: "dflt", targets: [(node: "T", property: text)]),
            (name: "command", type: string, default: "cmd.default", targets: [(node: "C", property: command)]),
            (name: "class", type: string, default: "", targets: [(node: "K", property: class)]),
            (name: "background", type: color, default: "#000000", targets: [(node: "B", property: background)]),
            (name: "border", type: color, default: "#000000", targets: [(node: "BC", property: border_color), (node: "BK", property: border_color)]),
            (name: "tint", type: color, default: "#FFFFFF", targets: [(node: "I", property: tint)]),
            (name: "opacity", type: number, default: 1.0, targets: [(node: "O", property: opacity)]),
            (name: "width", type: number, default: 10, targets: [(node: "S", property: width)]),
            (name: "height", type: number, default: 20, targets: [(node: "S", property: height)]),
            (name: "progress", type: number, default: 0.5, targets: [(node: "P", property: progress)]),
            (name: "visible", type: bool, default: true, targets: [(node: "V", property: visible)]),
            (name: "disabled", type: bool, default: false, targets: [(node: "D", property: disabled)]),
        ],
    ),
    root: (name: "All", classes: ["base"], style: (direction: row), children: [
        (name: "T", text: (spans: ["a", "b"], align: center)),
        (name: "C"),
        (name: "K", classes: ["own"]),
        (name: "B"),
        (name: "BC"),
        (name: "BK", border: (color: "#101010", width: 3.0, radius: 2.0)),
        (name: "I", image: (icon: "gear", tint: "#808080")),
        (name: "O"),
        (name: "S"),
        (name: "P", widget: progress(0.0)),
        (name: "V", style: (direction: column)),
        (name: "D"),
    ]),
)"##;

fn expand_all_props(instance: &str) -> UiNodeDef {
    let m = UiMigrators::default();
    let mut load = text_loader(files(vec![("ui/all.ui.ron".into(), ALL_PROPS.into())]), &m);
    let file = doc(&format!("(version: 9, root: (children: [{instance}]))"));
    let out = expand(&file, "ui/hud.ui.ron", &mut load);
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    out.root.children[0].clone()
}

/// Each property of the § 2.1 table changes exactly what the table says on its target node.
#[test]
fn every_parameter_property_has_its_documented_effect() {
    let all = expand_all_props(
        r##"(use: "ui/all.ui.ron", params: {
            "text": "Hello", "command": "game.quit", "class": "danger", "background": "#FF0000",
            "border": "#00FF00", "tint": "#0000FF", "opacity": 0.25, "width": 64, "height": 32,
            "progress": 2.5, "visible": false, "disabled": true,
        })"##,
    );
    // text: one literal span, align kept.
    let t = child(&all, "T");
    assert_eq!(literal(t), "Hello");
    assert_eq!(
        t.text.as_ref().unwrap().align,
        regia_ui::text::TextAlign::Center,
        "the text keeps its alignment"
    );
    // command.
    assert_eq!(child(&all, "C").command.as_deref(), Some("game.quit"));
    // class: appended to the node's own classes.
    assert_eq!(child(&all, "K").classes, ["own", "danger"]);
    // background: solid.
    assert_eq!(
        child(&all, "B").background,
        Some(BackgroundDef::Color(UiColor::from_hex("#FF0000").unwrap()))
    );
    // border_color: a 1 px border when the node had none; an existing border keeps width/radius.
    let bc = child(&all, "BC").border.as_ref().expect("a border was added");
    assert_eq!(bc.color, UiColor::from_hex("#00FF00").unwrap());
    assert_eq!(bc.width, 1.0);
    let bk = child(&all, "BK").border.as_ref().expect("border kept");
    assert_eq!(bk.color, UiColor::from_hex("#00FF00").unwrap());
    assert_eq!((bk.width, bk.radius), (3.0, 2.0));
    // tint.
    assert_eq!(
        child(&all, "I").image.as_ref().unwrap().tint,
        UiColor::from_hex("#0000FF").unwrap()
    );
    // opacity.
    assert_eq!(child(&all, "O").opacity, Some(0.25));
    // width, height: style.size in px.
    assert_eq!(
        child(&all, "S").style.size,
        [Dimension::Points(64.0), Dimension::Points(32.0)]
    );
    // progress: clamped to 0..1.
    assert!(matches!(child(&all, "P").widget, Some(WidgetDef::Progress(v)) if v == 1.0));
    // visible false: display none.
    assert_eq!(child(&all, "V").style.display, Display::None);
    // disabled.
    assert!(child(&all, "D").disabled);
}

/// Unset parameters write their defaults, a negative progress clamps to 0 and `visible: true`
/// leaves the node's display alone.
#[test]
fn defaults_apply_and_progress_clamps_low() {
    let all = expand_all_props(r#"(use: "ui/all.ui.ron", params: {"progress": -3.0})"#);
    assert_eq!(literal(child(&all, "T")), "dflt");
    assert_eq!(child(&all, "C").command.as_deref(), Some("cmd.default"));
    assert_eq!(child(&all, "K").classes, ["own"], "an empty class adds nothing");
    assert_eq!(child(&all, "O").opacity, Some(1.0));
    assert_eq!(
        child(&all, "S").style.size,
        [Dimension::Points(10.0), Dimension::Points(20.0)]
    );
    assert!(matches!(child(&all, "P").widget, Some(WidgetDef::Progress(v)) if v == 0.0));
    let v = child(&all, "V");
    assert_ne!(v.style.display, Display::None);
    assert_eq!(v.style, doc(ALL_PROPS).root.children[10].style, "style untouched");
    assert!(!child(&all, "D").disabled);
}

/// An instance's `name` replaces the component root's name, its `classes` are appended and its
/// `style` replaces the root's style only when set (§ 2.2).
#[test]
fn instance_name_classes_and_style_apply_to_the_component_root() {
    let placed = expand_all_props(
        r#"(use: "ui/all.ui.ron", name: "Mine", classes: ["extra"], style: (direction: column, size: (50, 60)))"#,
    );
    assert_eq!(placed.name.as_deref(), Some("Mine"));
    assert_eq!(placed.classes, ["base", "extra"]);
    assert_eq!(
        placed.style,
        doc(r#"(version: 9, root: (style: (direction: column, size: (50, 60))))"#)
            .root
            .style
    );

    let plain = expand_all_props(r#"(use: "ui/all.ui.ron")"#);
    assert_eq!(plain.name.as_deref(), Some("All"), "no name keeps the root's");
    assert_eq!(plain.classes, ["base"]);
    assert_eq!(
        plain.style,
        doc(ALL_PROPS).root.style,
        "no style keeps the root's"
    );
}

/// Exactly `MAX_COMPONENT_INSTANCES` direct instances expand; one more is a single
/// `ui_component_too_large` placeholder at the end, and the rest of the document still expands.
#[test]
fn the_instance_cap_is_exactly_4096() {
    assert_eq!(MAX_COMPONENT_INSTANCES, 4096);
    let leaf = r#"(version: 9, root: (name: "Leaf"))"#;
    let run = |n: usize| {
        let m = UiMigrators::default();
        let mut load = text_loader(files(vec![("ui/leaf.ui.ron".into(), leaf.into())]), &m);
        let kids = vec![r#"(use: "ui/leaf.ui.ron")"#; n].join(",");
        let file = doc(&format!(
            r#"(version: 9, root: (children: [{kids}, (name: "Tail")]))"#
        ));
        expand(&file, "ui/many.ui.ron", &mut load)
    };

    let at_cap = run(MAX_COMPONENT_INSTANCES);
    assert!(at_cap.diagnostics.is_empty(), "{:?}", &at_cap.diagnostics[..1]);
    assert!(
        at_cap.root.children[..MAX_COMPONENT_INSTANCES]
            .iter()
            .all(|c| c.name.as_deref() == Some("Leaf"))
    );

    let over = run(MAX_COMPONENT_INSTANCES + 1);
    assert_eq!(over.diagnostics.len(), 1, "{:?}", over.diagnostics);
    let d = &over.diagnostics[0];
    assert_eq!(d.code, "ui_component_too_large");
    assert_eq!(d.severity, "error");
    assert_eq!(d.component, "ui/leaf.ui.ron");
    assert_eq!(d.node, format!("#{MAX_COMPONENT_INSTANCES}"));
    let last = &over.root.children[MAX_COMPONENT_INSTANCES];
    assert_eq!(last.name.as_deref(), Some(PLACEHOLDER_NAME));
    assert_eq!(last.classes, [PLACEHOLDER_CLASS]);
    assert_eq!(literal(last), "ui_component_too_large: ui/leaf.ui.ron");
    assert_eq!(
        over.root.children[MAX_COMPONENT_INSTANCES - 1].name.as_deref(),
        Some("Leaf")
    );
    assert_eq!(
        over.root.children.last().unwrap().name.as_deref(),
        Some("Tail"),
        "plain nodes after the cap stay"
    );
}

/// Nested instances count towards the cap: components that each use eight others (8 + 64 + 512 +
/// 4,096 = 4,680 instances) expand exactly 4,096 and turn every other instance into a
/// `ui_component_too_large` placeholder, so the tree cannot grow exponentially.
#[test]
fn nested_fan_out_counts_towards_the_cap() {
    let fan = |name: &str, inner: &str| {
        let kids = vec![format!(r#"(use: "{inner}")"#); 8].join(",");
        format!(r#"(version: 9, root: (name: "{name}", children: [{kids}]))"#)
    };
    let list = vec![
        ("ui/l1.ui.ron".to_owned(), fan("L1", "ui/l2.ui.ron")),
        ("ui/l2.ui.ron".to_owned(), fan("L2", "ui/l3.ui.ron")),
        ("ui/l3.ui.ron".to_owned(), fan("L3", "ui/leaf.ui.ron")),
        (
            "ui/leaf.ui.ron".to_owned(),
            r#"(version: 9, root: (name: "Leaf"))"#.to_owned(),
        ),
    ];
    let m = UiMigrators::default();
    let mut load = text_loader(files(list), &m);
    let top = doc(&fan("Top", "ui/l1.ui.ron"));
    let out = expand(&top, "ui/top.ui.ron", &mut load);

    fn count(node: &UiNodeDef, expanded: &mut usize, placeholders: &mut usize) {
        match node.name.as_deref() {
            Some("L1" | "L2" | "L3" | "Leaf") => *expanded += 1,
            Some(PLACEHOLDER_NAME) => *placeholders += 1,
            _ => {}
        }
        for c in &node.children {
            count(c, expanded, placeholders);
        }
    }
    let (mut expanded, mut placeholders) = (0, 0);
    count(&out.root, &mut expanded, &mut placeholders);
    assert_eq!(expanded, MAX_COMPONENT_INSTANCES);
    assert!(placeholders > 0);
    assert_eq!(out.diagnostics.len(), placeholders);
    assert!(out.diagnostics.iter().all(|d| d.code == "ui_component_too_large"));
    assert!(
        expanded + placeholders < 8 + 64 + 512 + 4096,
        "failed subtrees are not walked"
    );
}

/// § 2.1 declaration check: duplicate parameter names and a `tint` target on a node without an
/// image are rejected; § 2.2: `use` must be a relative `/`-separated `.ui.ron` path under `assets/`.
#[test]
fn declarations_and_use_paths_follow_the_rules() {
    use regia_ui::ui_component::{check_component, check_use_path};
    let duplicate = doc(r#"(version: 9, component: (params: [
            (name: "a", type: string, default: ""), (name: "a", type: number, default: 1),
        ]), root: ())"#);
    assert!(check_component(&duplicate).is_err(), "names are unique");
    let tint_without_image = doc(r##"(version: 9, component: (params: [
            (name: "t", type: color, default: "#FFFFFF", targets: [(node: "N", property: tint)]),
        ]), root: (name: "N"))"##);
    assert!(
        check_component(&tint_without_image).is_err(),
        "a tint target needs an image"
    );
    let ok = doc(r##"(version: 9, component: (params: [
            (name: "t", type: color, default: "#FFFFFF", targets: [(node: "N", property: tint)]),
        ]), root: (name: "N", image: (icon: "gear")))"##);
    assert!(check_component(&ok).is_ok());

    assert!(check_use_path("ui/health_bar.ui.ron").is_ok());
    for bad in [
        "",
        "/ui/a.ui.ron",
        "C:/ui/a.ui.ron",
        "ui\\a.ui.ron",
        "ui/../../a.ui.ron",
        "ui//a.ui.ron",
        "ui/a.ron",
    ] {
        assert!(check_use_path(bad).is_err(), "{bad:?}");
    }
}

/// § 3: a nested instance's `$`-relative `bind_root` resolves against the outer instance's root,
/// and § 4 step 2: slot content is expanded in the outer document's context, so its `$` is the
/// outer component's root, not the inner component's.
#[test]
fn nested_bind_roots_and_slot_content_resolve_in_the_outer_context() {
    let inner = r#"(version: 9, component: (slots: ["s"]), root: (name: "Inner", children: [
        (name: "Own", text: (spans: [""]), bind: (source: "$.v")),
        (name: "Out", slot: "s"),
    ]))"#;
    let outer = r#"(version: 9, root: (name: "Outer", children: [
        (use: "ui/inner.ui.ron", bind_root: "$.child", slots: {"s": [
            (name: "Slotted", text: (spans: [""]), bind: (source: "$.w")),
        ]}),
    ]))"#;
    let m = UiMigrators::default();
    let mut load = text_loader(
        files(vec![
            ("ui/inner.ui.ron".into(), inner.into()),
            ("ui/outer.ui.ron".into(), outer.into()),
        ]),
        &m,
    );
    let file = doc(r#"(version: 9, root: (children: [(use: "ui/outer.ui.ron", bind_root: "Party.p1")]))"#);
    let out = expand(&file, "ui/hud.ui.ron", &mut load);
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    let outer = &out.root.children[0];
    let inner = child(outer, "Inner");
    assert_eq!(
        child(inner, "Own").bind.as_ref().unwrap().source,
        "Party.p1.child.v",
        "nested bind_root `$.child` is relative to the outer root"
    );
    let slotted = child(child(inner, "Out"), "Slotted");
    assert_eq!(
        slotted.bind.as_ref().unwrap().source,
        "Party.p1.w",
        "slot content binds against the outer component's root"
    );
    assert!(inner.slot.is_none() && child(inner, "Out").slot.is_none());
    assert_eq!(
        out.uses.iter().collect::<Vec<_>>(),
        ["ui/inner.ui.ron", "ui/outer.ui.ron"],
        "uses lists components reached through other components"
    );
}
