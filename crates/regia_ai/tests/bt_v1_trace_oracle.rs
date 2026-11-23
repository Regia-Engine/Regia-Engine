// SPDX-License-Identifier: MPL-2.0
//! Bounded differential oracle for the documented legacy control-flow contract.
//!
//! Sources: `docs/specs/22-tier2.md` T2.2, `docs/formats/bt-ron.md`,
//! `book/agents/ai.md` (parallel latches), `tree::ParallelPolicy`, the documented
//! `NodeState::running_child`, and `BtTask::reset` (parallel completion).
//! Selector/sequence resume the running child, then restart at the first child
//! after completion. Parallel visits all unfinished children in authored order,
//! latches terminal results, and clears descendant control state on completion.
//! We compare tick calls only: reset callback order/count is not a new promise.
//!
//! Grammar: leaf, inverter(child), or one of four ordered binary composites.
//! Exhaustive within this grammar: root depth 0, depth <= 3, nodes <= 7.
//! 764 shapes, with 4/56/384/320 having 1/2/3/4 leaves. All independent
//! assignments from six six-tick scripts give 499,704 cases, 2,998,224 ticks.
//! Empty composites already have basic coverage; ternary mixed results
//! and explicit trace witnesses below extend the generated binary grammar.
//! No timers, observers, callback traces, random inputs, files, or wall clocks.

use regia_ai::{
    BehaviorTree, BehaviorTreeBuilder, BtContext, BtStatus, BtTask, BtTaskRegistry, ParallelPolicy,
};
use regia_app::prelude::*;

use BtStatus::{Failure as F, Running as R, Success as S};

const TICKS: usize = 6;
const SCRIPTS: [[BtStatus; TICKS]; 6] = [
    [S, S, S, S, S, S],
    [F, F, F, F, F, F],
    [R, R, R, R, R, R],
    [R, S, F, R, S, F],
    [R, F, S, R, F, S],
    [R, R, S, R, R, F],
];

#[derive(Clone, Copy, Debug)]
enum Op {
    Selector,
    Sequence,
    All,
    One,
}

const OPS: [Op; 4] = [Op::Selector, Op::Sequence, Op::All, Op::One];

#[derive(Clone, Debug)]
enum Shape {
    Leaf,
    Invert(Box<Shape>),
    Branch(Op, Vec<Shape>),
}

impl Shape {
    fn leaves(&self) -> usize {
        match self {
            Self::Leaf => 1,
            Self::Invert(child) => child.leaves(),
            Self::Branch(_, children) => children.iter().map(Self::leaves).sum(),
        }
    }

    fn depth(&self) -> usize {
        match self {
            Self::Leaf => 0,
            Self::Invert(child) => 1 + child.depth(),
            Self::Branch(_, children) => 1 + children.iter().map(Self::depth).max().unwrap_or(0),
        }
    }
}

// Enumerate by exact size; smaller depth tables supply every ordered partition.
fn shapes(depth: usize, nodes: usize) -> Vec<Shape> {
    if nodes == 1 {
        return vec![Shape::Leaf];
    }
    if depth == 0 {
        return Vec::new();
    }
    let mut result: Vec<_> = shapes(depth - 1, nodes - 1)
        .into_iter()
        .map(|child| Shape::Invert(Box::new(child)))
        .collect();
    for left_nodes in 1..nodes - 1 {
        let left = shapes(depth - 1, left_nodes);
        let right = shapes(depth - 1, nodes - 1 - left_nodes);
        for op in OPS {
            for a in &left {
                for b in &right {
                    result.push(Shape::Branch(op, vec![a.clone(), b.clone()]));
                }
            }
        }
    }
    result
}

#[derive(Resource, Default)]
struct Input {
    scripts: Vec<usize>,
    tick: usize,
    calls: Vec<(usize, BtStatus)>,
}

struct ScriptedLeaf(usize);

impl BtTask for ScriptedLeaf {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let mut input = ctx.world.resource_mut::<Input>();
        let status = SCRIPTS[input.scripts[self.0]][input.tick];
        input.calls.push((self.0, status));
        status
    }
    // The scripted status depends on the fixed tick, not task-local progress.
    // Leave reset as the documented default; do not assert callback details.
}

fn compile_shape(
    shape: &Shape,
    builder: &mut BehaviorTreeBuilder,
    registry: &mut BtTaskRegistry,
    next_leaf: &mut usize,
) -> usize {
    match shape {
        Shape::Leaf => {
            let id = *next_leaf;
            *next_leaf += 1;
            let path = format!("oracle::leaf_{id}");
            registry.register_factory(path.clone(), move || Box::new(ScriptedLeaf(id)));
            builder.leaf(format!("leaf_{id}"), path)
        }
        Shape::Invert(child) => {
            let child = compile_shape(child, builder, registry, next_leaf);
            builder.inverter("invert", child)
        }
        Shape::Branch(op, children) => {
            let children = children
                .iter()
                .map(|child| compile_shape(child, builder, registry, next_leaf))
                .collect();
            match op {
                Op::Selector => builder.selector("selector", children),
                Op::Sequence => builder.sequence("sequence", children),
                Op::All => builder.parallel("all", ParallelPolicy::RequireAll, children),
                Op::One => builder.parallel("one", ParallelPolicy::RequireOne, children),
            }
        }
    }
}

// The reference is recursive and stores completed-child receipts, not an arena
// or a resume cursor. Serial nodes skip their completed prefix; parallel nodes
// skip any completed child. Only parallel completion discards descendant work.
#[derive(Clone)]
enum Reference {
    Leaf(usize),
    Invert(Box<Reference>),
    Branch {
        op: Op,
        children: Vec<Reference>,
        receipts: Vec<Option<BtStatus>>,
    },
}

impl Reference {
    fn new(shape: &Shape, next_leaf: &mut usize) -> Self {
        match shape {
            Shape::Leaf => {
                let id = *next_leaf;
                *next_leaf += 1;
                Self::Leaf(id)
            }
            Shape::Invert(child) => Self::Invert(Box::new(Self::new(child, next_leaf))),
            Shape::Branch(op, children) => Self::Branch {
                op: *op,
                receipts: vec![None; children.len()],
                children: children.iter().map(|c| Self::new(c, next_leaf)).collect(),
            },
        }
    }

    fn discard_work(&mut self) {
        match self {
            Self::Leaf(_) => {}
            Self::Invert(child) => child.discard_work(),
            Self::Branch {
                children, receipts, ..
            } => {
                receipts.fill(None);
                for child in children {
                    child.discard_work();
                }
            }
        }
    }

    fn tick(&mut self, scripts: &[usize], tick: usize, calls: &mut Vec<(usize, BtStatus)>) -> BtStatus {
        match self {
            Self::Leaf(id) => {
                let status = SCRIPTS[scripts[*id]][tick];
                calls.push((*id, status));
                status
            }
            Self::Invert(child) => match child.tick(scripts, tick, calls) {
                S => F,
                F => S,
                R => R,
            },
            Self::Branch {
                op,
                children,
                receipts,
            } => {
                let serial = matches!(op, Op::Selector | Op::Sequence);
                let advance = if matches!(op, Op::Selector | Op::One) {
                    F
                } else {
                    S
                };
                for (child, receipt) in children.iter_mut().zip(receipts.iter_mut()) {
                    if receipt.is_some() {
                        continue;
                    }
                    let status = child.tick(scripts, tick, calls);
                    if status != R {
                        *receipt = Some(status);
                    }
                    if serial && status != advance {
                        if status != R {
                            receipts.fill(None);
                        }
                        return status;
                    }
                }
                let result = if receipts.iter().any(|r| matches!(r, Some(s) if *s != advance)) {
                    if advance == S { F } else { S }
                } else if receipts.iter().all(Option::is_some) {
                    advance
                } else {
                    R
                };
                if result != R {
                    receipts.fill(None);
                    if !serial {
                        for child in children {
                            child.discard_work();
                        }
                    }
                }
                result
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Frame {
    status: BtStatus,
    calls: Vec<(usize, BtStatus)>,
}

fn compare(shape: &Shape, assignments: impl IntoIterator<Item = Vec<usize>>) -> Vec<Frame> {
    let mut builder = BehaviorTreeBuilder::new();
    let mut registry = BtTaskRegistry::default();
    let root = compile_shape(shape, &mut builder, &mut registry, &mut 0);
    let def = builder.build(root);
    let initial = Reference::new(shape, &mut 0);
    let mut world = World::new();
    world.insert_resource(Input::default());
    let entity = world.spawn_empty().id();
    let mut frames = Vec::new();
    for scripts in assignments {
        assert_eq!(scripts.len(), shape.leaves());
        let mut reference = initial.clone();
        let mut runtime = BehaviorTree::compile(&def, &registry).expect("generated valid tree");
        world.resource_mut::<Input>().scripts.clone_from(&scripts);
        frames.clear();
        for tick in 0..TICKS {
            let mut expected_calls = Vec::new();
            let expected = reference.tick(&scripts, tick, &mut expected_calls);
            world.resource_mut::<Input>().tick = tick;
            let actual = runtime.tick(&mut BtContext {
                world: &mut world,
                entity,
                dt: 1.0 / 60.0,
            });
            let calls = std::mem::take(&mut world.resource_mut::<Input>().calls);
            assert_eq!(
                (actual, &calls),
                (expected, &expected_calls),
                "counterexample: shape={shape:?}, scripts={scripts:?}, tick={tick}, prefix={frames:?}"
            );
            assert_eq!(runtime.last_status, actual);
            frames.push(Frame {
                status: actual,
                calls,
            });
        }
    }
    frames
}

#[test]
fn bounded_legacy_trees_match_the_recursive_trace_oracle() {
    let mut by_leaves = [0; 5];
    let mut cases = 0;
    for nodes in 1..=7 {
        for shape in shapes(3, nodes) {
            assert!(shape.depth() <= 3);
            let leaves = shape.leaves();
            by_leaves[leaves] += 1;
            let count = SCRIPTS.len().pow(leaves as u32);
            cases += count;
            compare(
                &shape,
                (0..count).map(|mut code| {
                    (0..leaves)
                        .map(|_| {
                            let script = code % SCRIPTS.len();
                            code /= SCRIPTS.len();
                            script
                        })
                        .collect()
                }),
            );
        }
    }
    assert_eq!(by_leaves, [0, 4, 56, 384, 320]);
    assert_eq!(cases, 499_704);
    assert_eq!(cases * TICKS, 2_998_224);
}

#[test]
fn resumption_skips_the_prefix_and_completion_restarts_it() {
    // Extends tree::tests::{sequence_short_circuits_on_failure,
    // selector_tries_children_until_one_succeeds} with an ordered, six-tick
    // witness. Starting at child zero on tick 1 adds an observable extra call.
    for (op, prefix_script, prefix_status, terminal) in [(Op::Sequence, 0, S, F), (Op::Selector, 1, F, S)] {
        let shape = Shape::Branch(op, vec![Shape::Leaf, Shape::Leaf]);
        let frames = compare(&shape, [vec![prefix_script, 3]]);
        let expected = [
            Frame {
                status: R,
                calls: vec![(0, prefix_status), (1, R)],
            },
            Frame {
                status: S,
                calls: vec![(1, S)],
            },
            Frame {
                status: F,
                calls: vec![(0, prefix_status), (1, F)],
            },
            Frame {
                status: R,
                calls: vec![(0, prefix_status), (1, R)],
            },
            Frame {
                status: S,
                calls: vec![(1, S)],
            },
            Frame {
                status: F,
                calls: vec![(0, prefix_status), (1, F)],
            },
        ];
        assert_eq!(frames, expected);
        assert!(frames.iter().any(|frame| frame.status == terminal));
    }
}

#[test]
fn parallel_completion_discards_a_nested_serial_prefix() {
    // Extends a_finished_parallel_halts_the_running_siblings: on tick 1 the
    // nested sequence/selector must call its prefix again after being aborted.
    for (parallel, serial, prefix, decision, terminal) in
        [(Op::One, Op::Sequence, 0, 0, S), (Op::All, Op::Selector, 1, 1, F)]
    {
        let shape = Shape::Branch(
            parallel,
            vec![Shape::Branch(serial, vec![Shape::Leaf, Shape::Leaf]), Shape::Leaf],
        );
        let frames = compare(&shape, [vec![prefix, 2, decision]]);
        for frame in frames {
            assert_eq!(frame.status, terminal);
            assert_eq!(frame.calls, vec![(0, SCRIPTS[prefix][0]), (1, R), (2, terminal)]);
        }
    }
}

#[test]
fn parallel_latches_survive_mixed_results_until_completion_then_clear() {
    // Extends parallel_does_not_retick_a_finished_child for both policies,
    // three children, ordered calls, changing scripts and a subsequent run.
    for (op, latched, changing, held) in [(Op::All, 0, 3, S), (Op::One, 1, 4, F)] {
        let shape = Shape::Branch(op, vec![Shape::Leaf, Shape::Leaf, Shape::Leaf]);
        let frames = compare(&shape, [vec![latched, changing, 5]]);
        // Child 1's tick-1 result must remain latched at tick 2, when its
        // script flips to the decisive opposite result. Both policies then
        // start a fresh run at tick 3 and finish with Failure at tick 5.
        let started = || Frame {
            status: R,
            calls: vec![(0, held), (1, R), (2, R)],
        };
        let waiting = || Frame {
            status: R,
            calls: vec![(1, held), (2, R)],
        };
        assert_eq!(
            frames,
            [
                started(),
                waiting(),
                Frame {
                    status: S,
                    calls: vec![(2, S)],
                },
                started(),
                waiting(),
                Frame {
                    status: F,
                    calls: vec![(2, F)],
                },
            ]
        );
    }
}

#[test]
fn decisive_parallel_results_still_visit_every_unfinished_child_in_order() {
    // The existing policy tests count calls for a mixed three-child parallel.
    // Extend that to exact identity/order for every permutation, both policies,
    // and six consecutive completed runs (12 cases, 72 ticks).
    for op in [Op::All, Op::One] {
        for scripts in [
            vec![0, 1, 2],
            vec![0, 2, 1],
            vec![1, 0, 2],
            vec![1, 2, 0],
            vec![2, 0, 1],
            vec![2, 1, 0],
        ] {
            let shape = Shape::Branch(op, vec![Shape::Leaf; 3]);
            let frames = compare(&shape, [scripts.clone()]);
            for frame in frames {
                assert_eq!(frame.status, if matches!(op, Op::All) { F } else { S });
                assert_eq!(
                    frame.calls,
                    scripts
                        .iter()
                        .enumerate()
                        .map(|(id, &script)| (id, SCRIPTS[script][0]))
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}
