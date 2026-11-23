// SPDX-License-Identifier: MPL-2.0
//! The query language's acceptance tests: a table of 25 queries on a fixture level, the documented
//! errors, today's search tokens, a 10 000-case parser fuzz and a print/parse property test.

use super::*;
use crate::outliner::{HierarchyNode, hierarchy_tree};
use regia_core::math::Vec3;
use regia_scene::components::Tags;
use regia_scene::prelude::{EntityGuid, Name, Transform};

/// The fixture level:
///
/// ```text
/// Crate            kind Crate, tags loot heavy, y -2
/// Door Back        y 3
/// Door Front       tags loot, y 0
/// Lights
///   Porch Lamp     PointLight 400, tags porch, y 2
///   Street
///     Lamp Post    PointLight 2500, y -1
///   Street Lamp    PointLight 1500, tags street, y 0
/// Sun              DirectionalLight 90000
/// ```
fn fixture() -> regia_app::App {
    use regia_scene::kind::{KindInfo, KindRegistry, KindSource, KindTag};
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<regia_render::PointLight>();
    app.register_type::<regia_render::DirectionalLight>();
    let world = app.world_mut();
    world.resource_mut::<KindRegistry>().register_data(KindInfo {
        name: "Crate".into(),
        icon: "box".into(),
        color: [1, 2, 3],
        description: String::new(),
        category: Some("Props".into()),
        required: Vec::new(),
        source: KindSource::Data {
            path: "assets/kinds/Crate.kind.ron".into(),
            marker: None,
        },
    });
    let at = |y: f32| Transform::from_translation(Vec3::new(0.0, y, 0.0));
    let lamp = |intensity: f32| regia_render::PointLight {
        intensity,
        cast_shadows: false,
        ..Default::default()
    };
    world.spawn((
        Name::new("Crate"),
        at(-2.0),
        KindTag("Crate".into()),
        Tags::from_iter_str(["loot", "heavy"]),
        EntityGuid::new_random(),
    ));
    world.spawn((Name::new("Door Back"), at(3.0), EntityGuid::new_random()));
    world.spawn((
        Name::new("Door Front"),
        at(0.0),
        Tags::from_iter_str(["loot"]),
        EntityGuid::new_random(),
    ));
    let lights = world
        .spawn((Name::new("Lights"), at(0.0), EntityGuid::new_random()))
        .id();
    world.spawn((
        Name::new("Porch Lamp"),
        at(2.0),
        lamp(400.0),
        Tags::from_iter_str(["porch"]),
        ChildOf(lights),
        EntityGuid::new_random(),
    ));
    let street = world
        .spawn((
            Name::new("Street"),
            at(0.0),
            ChildOf(lights),
            EntityGuid::new_random(),
        ))
        .id();
    world.spawn((
        Name::new("Lamp Post"),
        at(-1.0),
        lamp(2500.0),
        ChildOf(street),
        EntityGuid::new_random(),
    ));
    world.spawn((
        Name::new("Street Lamp"),
        at(0.0),
        lamp(1500.0),
        Tags::from_iter_str(["street"]),
        ChildOf(lights),
        EntityGuid::new_random(),
    ));
    world.spawn((
        Name::new("Sun"),
        at(10.0),
        regia_render::DirectionalLight {
            illuminance: 90_000.0,
            ..Default::default()
        },
        EntityGuid::new_random(),
    ));
    app
}

/// The labels of every row `query` matches itself (ancestors kept for context are not counted),
/// sorted.
fn matching(world: &World, tree: &[HierarchyNode], query: &Query) -> Vec<String> {
    fn walk(
        world: &World,
        node: &HierarchyNode,
        ancestors: &mut Vec<String>,
        query: &Query,
        out: &mut Vec<String>,
    ) {
        let chain: Vec<&str> = ancestors.iter().map(String::as_str).collect();
        let facts = RowFacts {
            entity: node.entity,
            label: &node.label,
            icon: &node.icon,
            components: &node.components,
            tags: &node.tags,
            ancestors: &chain,
        };
        if query.matches(&facts, world) {
            out.push(node.label.clone());
        }
        ancestors.push(node.label.clone());
        for child in &node.children {
            walk(world, child, ancestors, query, out);
        }
        ancestors.pop();
    }
    let mut out = Vec::new();
    for node in tree {
        walk(world, node, &mut Vec::new(), query, &mut out);
    }
    out.sort();
    out
}

/// The acceptance table: 25 queries over every term kind, operator and combinator.
#[test]
fn twenty_five_queries_on_the_fixture_level() {
    let app = fixture();
    let world = app.world();
    let tree = hierarchy_tree(world);
    let table: &[(&str, &[&str])] = &[
        ("lamp", &["Lamp Post", "Porch Lamp", "Street Lamp"]),
        ("name~door", &["Door Back", "Door Front"]),
        ("Name~DOOR", &["Door Back", "Door Front"]),
        ("name=street", &["Street"]),
        ("kind:crate", &["Crate"]),
        ("kind:none door", &["Door Back", "Door Front"]),
        ("type:PointLight", &["Lamp Post", "Porch Lamp", "Street Lamp"]),
        ("has:DirectionalLight", &["Sun"]),
        ("tag:loot", &["Crate", "Door Front"]),
        ("tag:loot and not kind:crate", &["Door Front"]),
        ("PointLight.intensity>1000", &["Lamp Post", "Street Lamp"]),
        ("Light.intensity>1000", &["Lamp Post", "Street Lamp"]),
        ("PointLight.intensity<=400", &["Porch Lamp"]),
        (
            "Transform.translation.y<=0",
            &[
                "Crate",
                "Door Front",
                "Lamp Post",
                "Lights",
                "Street",
                "Street Lamp",
            ],
        ),
        ("Transform.translation.y<0", &["Crate", "Lamp Post"]),
        ("DirectionalLight.illuminance>=90000", &["Sun"]),
        (
            "under:Lights",
            &["Lamp Post", "Porch Lamp", "Street", "Street Lamp"],
        ),
        ("under:Lights/Street", &["Lamp Post"]),
        ("under:/Street", &[]),
        ("parent:Lights", &["Porch Lamp", "Street", "Street Lamp"]),
        ("tag:porch or tag:street", &["Porch Lamp", "Street Lamp"]),
        ("(door or crate) and tag:loot", &["Crate", "Door Front"]),
        ("not (under:Lights or has:Transform)", &[]),
        ("lamp and not under:Street", &["Porch Lamp", "Street Lamp"]),
        ("\"door front\" or PointLight.cast_shadows=true", &["Door Front"]),
    ];
    assert_eq!(table.len(), 25);
    for (text, expected) in table {
        let query = Query::parse(text).unwrap_or_else(|e| panic!("{text:?} parses: {e}"));
        assert_eq!(
            matching(world, &tree, &query),
            expected.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            "{text}"
        );
    }
}

/// Malformed queries give a documented code and the column of the problem.
#[test]
fn malformed_queries_report_a_code_and_a_column() {
    let cases: &[(&str, &str, usize)] = &[
        ("(lamp", "query_unclosed_paren", 1),
        ("lamp)", "query_unexpected_paren", 5),
        ("name~\"door", "query_unclosed_quote", 6),
        ("lamp and", "query_missing_operand", 9),
        ("or lamp", "query_missing_operand", 1),
        ("not", "query_missing_operand", 4),
        ("()", "query_missing_operand", 2),
        ("has:", "query_missing_value", 5),
        ("under:", "query_missing_value", 7),
        ("PointLight.intensity>", "query_missing_value", 22),
        ("PointLight.intensity>bright", "query_not_a_number", 22),
        ("Point-Light.x=1", "query_bad_field", 1),
        ("a.=1", "query_bad_field", 1),
    ];
    for (text, code, column) in cases {
        let err = Query::parse(text).expect_err(text);
        assert_eq!((err.code, err.column), (*code, *column), "{text}: {err}");
        assert!(ERROR_CODES.contains(&err.code), "{} is documented", err.code);
    }
    let deep = format!("{}lamp{}", "(".repeat(MAX_DEPTH + 1), ")".repeat(MAX_DEPTH + 1));
    assert_eq!(Query::parse(&deep).map_err(|e| e.code), Err("query_too_deep"));
    let nots = format!("{}lamp", "not ".repeat(MAX_DEPTH + 1));
    assert_eq!(Query::parse(&nots).map_err(|e| e.code), Err("query_too_deep"));
    for code in ERROR_CODES {
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{code}"
        );
    }
}

/// Today's search cases (the old `filter_tree` tests) mean exactly what they meant.
#[test]
fn todays_tokens_keep_their_meaning() {
    let parsed = |text: &str| Query::parse(text).expect(text);
    let name = |text: &str| {
        Expr::Term(Term::Name {
            op: CompareOp::Contains,
            text: text.into(),
        })
    };
    assert!(parsed("").is_empty() && parsed("   ").is_empty());
    assert!(parsed("kind:").is_empty(), "a bare kind: is ignored");
    assert!(parsed("type: tag:").is_empty());
    assert_eq!(parsed("CHI").expr(), Some(&name("chi")));
    assert_eq!(
        parsed("crate lid").expr(),
        Some(&name("crate lid")),
        "bare words side by side are one phrase"
    );
    assert_eq!(
        parsed("kind:none su").expr(),
        Some(&Expr::And(vec![
            Expr::Term(Term::Kind(KindFilter::None)),
            name("su")
        ]))
    );
    assert_eq!(
        parsed("kind:a kind:B").expr(),
        Some(&Expr::Term(Term::Kind(KindFilter::Name("b".into())))),
        "a later kind: replaces the earlier one"
    );
    assert_eq!(
        parsed("kind:? x").expr(),
        Some(&Expr::And(vec![
            Expr::Term(Term::Kind(KindFilter::Unclassified)),
            name("x")
        ]))
    );
    assert_eq!(
        parsed("type:AudioEmitter tag:loot").expr(),
        Some(&Expr::And(vec![
            Expr::Term(Term::Has("audioemitter".into())),
            Expr::Term(Term::Tag("loot".into())),
        ]))
    );
    assert_eq!(
        parsed("12:30").expr(),
        Some(&name("12:30")),
        "an unknown key stays name text"
    );
    assert_eq!(
        parsed("door and front").expr(),
        Some(&Expr::And(vec![name("door"), name("front")])),
        "an explicit and splits the phrase"
    );
}

/// Today's `filter_tree` behavior through the new parser, on the fixture.
#[test]
fn filter_tree_keeps_matches_and_ancestors_with_the_new_parser() {
    let app = fixture();
    let tree = hierarchy_tree(app.world());
    fn labels(nodes: &[HierarchyNode], out: &mut Vec<String>) {
        for node in nodes {
            out.push(node.label.clone());
            labels(&node.children, out);
        }
    }
    let kept = |text: &str| {
        let mut out = Vec::new();
        labels(&crate::outliner::filter_tree(&tree, text), &mut out);
        out
    };
    assert_eq!(kept("post"), ["Lights", "Street", "Lamp Post"]);
    assert_eq!(
        kept("lamp or crate"),
        [
            "Crate",
            "Lights",
            "Porch Lamp",
            "Street",
            "Lamp Post",
            "Street Lamp"
        ]
    );
    assert_eq!(kept("zzz"), Vec::<String>::new());
    assert_eq!(kept("").len(), 9, "an empty query keeps everything");
    assert_eq!(
        kept("(lamp").len(),
        9,
        "a malformed query filters nothing (the panel shows the error)"
    );
    let world = app.world();
    let mut out = Vec::new();
    labels(
        &crate::outliner::filter_tree_query(
            &tree,
            &Query::parse("PointLight.intensity>2000").expect("parses"),
            world,
        ),
        &mut out,
    );
    assert_eq!(out, ["Lights", "Street", "Lamp Post"]);
}

/// A tiny deterministic generator (no new dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() as usize) % items.len()]
    }
}

/// 10 000 random queries from the language's own alphabet: parsing never panics, every error
/// has a documented code and a column inside the text (or just past it), and every query that
/// parses evaluates on the fixture without panicking.
#[test]
fn parser_fuzz_ten_thousand_cases_never_panics() {
    let app = fixture();
    let world = app.world();
    let tree = hierarchy_tree(world);
    let pieces = [
        "(",
        ")",
        " ",
        "and",
        "or",
        "not",
        "\"",
        ":",
        "~",
        "=",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        ".",
        "/",
        "kind",
        "type",
        "tag",
        "has",
        "under",
        "parent",
        "name",
        "lamp",
        "Lights",
        "PointLight",
        "intensity",
        "1000",
        "-1.5",
        "true",
        "?",
        "é",
        "\u{1f4a1}",
        "translation.y",
        "[0]",
        "x",
    ];
    let mut rng = Lcg(0x005e_ed0f_0e11);
    for _ in 0..10_000 {
        let len = 1 + rng.next() as usize % 12;
        let text: String = (0..len).map(|_| *rng.pick(&pieces)).collect();
        match Query::parse(&text) {
            Ok(query) => {
                let _ = matching(world, &tree, &query);
            }
            Err(err) => {
                assert!(ERROR_CODES.contains(&err.code), "{text:?}: {err}");
                assert!(
                    err.column >= 1 && err.column <= text.chars().count() + 1,
                    "{text:?}: {err}"
                );
            }
        }
    }
}

/// Property test: a random expression printed with full parentheses parses back to a query that
/// matches exactly the rows the expression matches (`and`, `or`, `not` and grouping mean what
/// they say), over 500 random expressions.
#[test]
fn printed_expressions_parse_back_to_the_same_matches() {
    let app = fixture();
    let world = app.world();
    let tree = hierarchy_tree(world);
    let atoms = [
        "lamp",
        "kind:crate",
        "tag:loot",
        "has:PointLight",
        "under:Lights",
        "parent:Street",
        "PointLight.intensity>1000",
        "Transform.translation.y<0",
        "name=sun",
        "name~o",
    ];
    fn build(rng: &mut Lcg, atoms: &[&str], depth: usize) -> (String, Expr) {
        let atom = |rng: &mut Lcg| {
            let text = *rng.pick(atoms);
            let expr = Query::parse(text)
                .expect("atom parses")
                .expr()
                .cloned()
                .expect("atom has a term");
            (text.to_string(), expr)
        };
        if depth == 0 {
            return atom(rng);
        }
        match rng.next() % 4 {
            0 => atom(rng),
            1 => {
                let (text, expr) = build(rng, atoms, depth - 1);
                (format!("not ({text})"), Expr::Not(Box::new(expr)))
            }
            2 => {
                let (a, ea) = build(rng, atoms, depth - 1);
                let (b, eb) = build(rng, atoms, depth - 1);
                (format!("({a}) and ({b})"), Expr::And(vec![ea, eb]))
            }
            _ => {
                let (a, ea) = build(rng, atoms, depth - 1);
                let (b, eb) = build(rng, atoms, depth - 1);
                (format!("({a}) or ({b})"), Expr::Or(vec![ea, eb]))
            }
        }
    }
    let mut rng = Lcg(42);
    for _ in 0..500 {
        let (text, expr) = build(&mut rng, &atoms, 4);
        let parsed = Query::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let direct = Query { expr: Some(expr) };
        assert_eq!(
            matching(world, &tree, &parsed),
            matching(world, &tree, &direct),
            "{text}"
        );
    }
}
