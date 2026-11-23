// SPDX-License-Identifier: MPL-2.0
//! Graph files are user data: mutated `.pgraph.ron` text must never panic the reader, the compiler
//! or the evaluator (it may only answer errors). Deterministic (xorshift64*, fixed seed).

mod common;

use common::{box_shape, snapshot};
use regia_procgen::exec::RunLimitsOverride;
use regia_procgen::{
    CancelToken, Evaluator, ProcGraph, ProcNodeRegistry, RunInputs, RunLimits, SettingValue,
};

const BASE: &str = r#"ProcGraph(
    format_version: 1,
    description: "fuzz base",
    params: [(name: "spacing", ty: Float, default: 4.0, min: Some(0.5), max: Some(64.0), doc: "")],
    inputs: [],
    outputs: [(name: "out", kinds: ["Points"])],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "sample.region", settings: {"spacing_m": Bind("spacing")}, at: (1.0, 0.0)),
        3: (ty: "points.jitter", settings: {"offset": [(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]}, at: (2.0, 0.0)),
        4: (ty: "density.noise", settings: {"scale_m": 8.0}, at: (3.0, 0.0)),
        5: (ty: "points.prune", settings: {}, at: (4.0, 0.0)),
        6: (ty: "spawn.mesh", settings: {"entries": [(mesh: "a.glb", weight: 2.0)]}, at: (5.0, 0.0)),
        7: (ty: "attr.math", settings: {"op": sqrt, "a": "density", "target": "w"}, at: (6.0, 0.0)),
        8: (ty: "graph.output", settings: {"pin": "out"}, at: (7.0, 0.0)),
    },
    edges: [(from: (1, "out"), to: (2, "region")), (from: (2, "out"), to: (3, "in")), (from: (3, "out"), to: (4, "in")),
        (from: (4, "out"), to: (5, "in")), (from: (5, "out"), to: (6, "in")), (from: (6, "out"), to: (7, "in")),
        (from: (7, "out"), to: (8, "in"))],
    flags: (starter: false, tool: None, domain: World),
)"#;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const PIECES: &[&str] = &[
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ",",
    ":",
    "\"",
    "-1",
    "1e308",
    "NaN",
    "inf",
    "0",
    "99999999",
    "Bind(\"x\")",
    "Some(",
    "None",
    "ty: \"gen.grid\"",
    "\"count\": (1e9, 1.0, 1e9)",
    "9",
    "//",
    "/*",
    "graph.call",
];

#[test]
fn mutated_graph_files_never_panic() {
    let reg = ProcNodeRegistry::with_builtins();
    let mut inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [20.0, 5.0, 20.0], 0.0), None), 3);
    inputs.limits = RunLimits::DESKTOP.with_override(&RunLimitsOverride {
        points_per_item: Some(20_000),
        total_elements: Some(200_000),
        ..RunLimitsOverride::default()
    });
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut parsed, mut compiled, mut ran) = (0, 0, 0);
    for _ in 0..1500 {
        let mut text: Vec<char> = BASE.chars().collect();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(text.len());
            match rng.below(3) {
                0 => {
                    text.remove(at);
                }
                1 => {
                    let piece = PIECES[rng.below(PIECES.len())];
                    for (k, c) in piece.chars().enumerate() {
                        text.insert(at + k, c);
                    }
                }
                _ => {
                    let end = (at + rng.below(40)).min(text.len());
                    text.drain(at..end);
                }
            }
        }
        let text: String = text.into_iter().collect();
        let _ = SettingValue::parse(&text);
        let Ok(g) = ProcGraph::from_ron(&text) else {
            continue;
        };
        parsed += 1;
        let _ = g.to_ron();
        let Ok(c) = g.compile(&reg) else {
            continue;
        };
        compiled += 1;
        if Evaluator::new().run(&c, &inputs, &CancelToken::new()).is_ok() {
            ran += 1;
        }
    }
    assert!(
        parsed > 50 && compiled > 10 && ran > 5,
        "the fuzz reached every stage: {parsed} {compiled} {ran}"
    );
}

/// The same pipeline on well-formed text whose values are hostile (ADR-1917): integer edges, NaN, huge counts and
/// repeated lists reach the compiler and the evaluator, which the syntax mutations above rarely get past the parser.
#[test]
fn hostile_graph_values_never_panic_or_escape_the_limits() {
    let reg = ProcNodeRegistry::with_builtins();
    let mut inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [20.0, 5.0, 20.0], 0.0), None), 3);
    inputs.limits = RunLimits::DESKTOP.with_override(&RunLimitsOverride {
        points_per_item: Some(20_000),
        total_elements: Some(200_000),
        ..RunLimitsOverride::default()
    });
    let mut rng = regia_core::fuzz::FuzzRng::new(0x0917_0001);
    let mut compiled = 0;
    for _ in 0..600 {
        let text = regia_core::fuzz::mutate_text_values(BASE, &mut rng);
        let Ok(g) = ProcGraph::from_ron(&text) else {
            continue;
        };
        let Ok(c) = g.compile(&reg) else {
            continue;
        };
        compiled += 1;
        let _ = Evaluator::new().run(&c, &inputs, &CancelToken::new());
    }
    assert!(
        compiled > 0,
        "no mutation compiled: the evaluator was never reached"
    );
}
