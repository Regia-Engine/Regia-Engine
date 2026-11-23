// SPDX-License-Identifier: MPL-2.0
//! Boundary coverage for the shared migration step limit.
//!
//! The oracle below walks transition data without calling `migrate` or sharing its callback
//! dispatch. The test callbacks only record their identity and a visible payload mutation.

use regia_core::version::{VersionMigrationError, VersionStep, migrate};

const LIMIT: usize = 32;

#[derive(Debug, Default)]
struct Payload {
    calls: Vec<usize>,
    mutations: Vec<usize>,
    fail_at: Option<usize>,
}

impl Payload {
    fn run(&mut self, id: usize) -> Result<(), String> {
        self.calls.push(id);
        self.mutations.push(id);
        if self.fail_at == Some(id) {
            Err(format!("callback {id} failed"))
        } else {
            Ok(())
        }
    }
}

macro_rules! callbacks {
    ($($name:ident = $id:literal),+ $(,)?) => {
        $(fn $name(payload: &mut Payload) -> Result<(), String> {
            payload.run($id)
        })+
        const CALLBACKS: [fn(&mut Payload) -> Result<(), String>; 33] = [$($name),+];
    };
}

callbacks!(
    callback_0 = 0,
    callback_1 = 1,
    callback_2 = 2,
    callback_3 = 3,
    callback_4 = 4,
    callback_5 = 5,
    callback_6 = 6,
    callback_7 = 7,
    callback_8 = 8,
    callback_9 = 9,
    callback_10 = 10,
    callback_11 = 11,
    callback_12 = 12,
    callback_13 = 13,
    callback_14 = 14,
    callback_15 = 15,
    callback_16 = 16,
    callback_17 = 17,
    callback_18 = 18,
    callback_19 = 19,
    callback_20 = 20,
    callback_21 = 21,
    callback_22 = 22,
    callback_23 = 23,
    callback_24 = 24,
    callback_25 = 25,
    callback_26 = 26,
    callback_27 = 27,
    callback_28 = 28,
    callback_29 = 29,
    callback_30 = 30,
    callback_31 = 31,
    callback_32 = 32,
);

#[derive(Debug, PartialEq, Eq)]
enum OracleResult {
    Reached(u32),
    Failed(VersionMigrationError),
}

/// Walks plain transition records and enforces the stated maximum number of callbacks.
fn oracle(
    mut version: u32,
    target: u32,
    transitions: &[(u32, u32, usize)],
    fail_at: Option<usize>,
) -> (OracleResult, Vec<usize>) {
    let mut calls = Vec::new();
    while version < target {
        if calls.len() == LIMIT {
            return (
                OracleResult::Failed(VersionMigrationError {
                    from: version,
                    to: target,
                    reason: "migration chain did not converge".to_owned(),
                }),
                calls,
            );
        }
        let Some(&(from, to, id)) = transitions.iter().find(|&&(from, _, _)| from == version) else {
            return (
                OracleResult::Failed(VersionMigrationError {
                    from: version,
                    to: target,
                    reason: format!("no migration registered for v{version}"),
                }),
                calls,
            );
        };
        calls.push(id);
        if fail_at == Some(id) {
            return (
                OracleResult::Failed(VersionMigrationError {
                    from,
                    to,
                    reason: format!("callback {id} failed"),
                }),
                calls,
            );
        }
        version = to;
    }
    (OracleResult::Reached(version), calls)
}

fn transitions(count: usize, sparse: bool) -> Vec<(u32, u32, usize)> {
    (0..count)
        .map(|id| {
            let from = if sparse { 100 + (id as u32) * 3 } else { id as u32 };
            let to = if sparse { from + 3 } else { from + 1 };
            (from, to, id)
        })
        .collect()
}

fn steps(transitions: &[(u32, u32, usize)]) -> Vec<VersionStep<Payload>> {
    transitions
        .iter()
        .map(|&(from, to, id)| VersionStep {
            from,
            to,
            migrate: CALLBACKS[id],
        })
        .collect()
}

fn assert_case(
    start: u32,
    target: u32,
    transitions: &[(u32, u32, usize)],
    fail_at: Option<usize>,
) -> (OracleResult, Payload) {
    let (expected, expected_calls) = oracle(start, target, transitions, fail_at);
    let mut payload = Payload {
        fail_at,
        ..Payload::default()
    };
    let actual = migrate(&mut payload, start, &steps(transitions), target);
    match (&expected, actual) {
        (OracleResult::Reached(version), Ok(actual_version)) => assert_eq!(*version, actual_version),
        (OracleResult::Failed(error), Err(actual_error)) => assert_eq!(*error, actual_error),
        (expected, actual) => panic!("oracle returned {expected:?}, migrate returned {actual:?}"),
    }
    assert_eq!(payload.calls, expected_calls, "exact callback prefix");
    assert_eq!(payload.mutations, expected_calls, "mutations are not rolled back");
    (expected, payload)
}

#[test]
fn permits_31_and_32_steps_and_refuses_a_33rd_before_its_callback() {
    for count in [31, 32] {
        let chain = transitions(count, false);
        let (result, payload) = assert_case(0, count as u32, &chain, None);
        assert_eq!(result, OracleResult::Reached(count as u32));
        assert_eq!(payload.calls, (0..count).collect::<Vec<_>>());
    }

    let chain = transitions(33, false);
    let (result, mut payload) = assert_case(0, 33, &chain, None);
    assert_eq!(
        result,
        OracleResult::Failed(VersionMigrationError {
            from: 32,
            to: 33,
            reason: "migration chain did not converge".to_owned(),
        })
    );
    assert_eq!(payload.calls, (0..32).collect::<Vec<_>>());

    // A rejected call leaves its visible payload prefix intact but does not poison a later call.
    payload.fail_at = None;
    let next = [(500, 501, 0)];
    assert_eq!(migrate(&mut payload, 500, &steps(&next), 501), Ok(501));
    assert_eq!(payload.calls, (0..32).chain([0]).collect::<Vec<_>>());
}

#[test]
fn sparse_versions_can_reach_the_target_on_the_last_allowed_step() {
    let chain = transitions(32, true);
    let start = chain[0].0;
    let target = chain[31].1;
    let (result, payload) = assert_case(start, target, &chain, None);
    assert_eq!(result, OracleResult::Reached(target));
    assert_eq!(payload.calls, (0..32).collect::<Vec<_>>());
}

#[test]
fn callback_failure_reports_its_edge_and_keeps_the_exact_mutated_prefix() {
    let chain = transitions(8, true);
    let (result, mut payload) = assert_case(chain[0].0, chain[7].1, &chain, Some(4));
    assert_eq!(
        result,
        OracleResult::Failed(VersionMigrationError {
            from: chain[4].0,
            to: chain[4].1,
            reason: "callback 4 failed".to_owned(),
        })
    );
    assert_eq!(payload.calls, [0, 1, 2, 3, 4]);
    assert_eq!(payload.mutations, [0, 1, 2, 3, 4]);

    // A migration error does not roll back callback mutations; a fresh invocation is independent.
    payload.fail_at = None;
    let next = [(900, 901, 7)];
    assert_eq!(migrate(&mut payload, 900, &steps(&next), 901), Ok(901));
    assert_eq!(payload.calls, [0, 1, 2, 3, 4, 7]);
}

#[test]
fn reordering_unique_from_entries_does_not_change_the_oracle_result() {
    let original = transitions(32, true);
    let mut reversed = original.clone();
    reversed.reverse();
    let start = original[0].0;
    let target = original[31].1;
    let (expected, expected_calls) = oracle(start, target, &original, None);
    let (reordered, reordered_calls) = oracle(start, target, &reversed, None);
    assert_eq!(expected, reordered);
    assert_eq!(expected_calls, reordered_calls);

    let mut payload = Payload::default();
    assert_eq!(
        migrate(&mut payload, start, &steps(&reversed), target),
        Ok(target)
    );
    assert_eq!(payload.calls, expected_calls);
}
