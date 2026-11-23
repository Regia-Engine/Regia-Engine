// SPDX-License-Identifier: MIT-0
//! The arena's smoke level as a functional test (spec 41 § 8 test 19, ADR-2755).
//!
//! `arena --smoke` installs the functional-test runner and places one test, `arena_smoke`, tagged
//! `smoke`. `smoke.session.ron` starts that process; `regia test session` waits for the agent,
//! runs the test through `functest.*` and stops the game. The binary calls `regia::run_with`, so
//! it opens a window. `REGIA_SESSION_RENDER=0` does not change that (spec 41 § 2.8). The test checks that the match
//! started (an `ArenaState` in warm-up or round, time advancing) and that the pawns spawned, then
//! passes after a fixed number of frames. It counts frames only: no clock.
//!
//! Only the binary installs it ([`install`]); the library's plugin and the determinism tests never
//! see the runner or the test entity.

use regia_app::prelude::*;
use regia_framework::functional::{
    FunctionalAppExt, FunctionalTest, FunctionalTestPlugin, TestBody, TestBodyRef, TestContext, Verdict,
};
use regia_scene::components::Name;

use crate::match_flow::{Phase, arena_state};
use crate::pawn::Pawn;

/// Frames the match runs before the test judges it.
pub const SMOKE_FRAMES: u64 = 90;

/// The smoke test body.
#[derive(Default)]
pub struct ArenaSmoke;

impl TestBody for ArenaSmoke {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.begin_step("match runs");
    }

    fn tick(&mut self, cx: &mut TestContext) {
        if cx.frame() < SMOKE_FRAMES {
            return;
        }
        let (phase, elapsed) = match arena_state(cx.world()) {
            Some(state) => (Some(state.phase), state.elapsed),
            None => (None, 0.0),
        };
        cx.is_true(phase.is_some(), "the match state exists");
        cx.is_true(
            matches!(phase, Some(Phase::Warmup | Phase::Round)),
            "the match is in warm-up or the round",
        );
        cx.is_true(elapsed > 0.0, "match time advances");
        let pawns = cx.world().query::<&Pawn>().iter(cx.world()).count();
        cx.is_true(pawns > 0, "pawns spawned");
        cx.end_step();
        if cx.failure_count() == 0 {
            cx.conclude(Verdict::Passed, "the arena match started and ran");
        } else {
            cx.conclude(Verdict::Failed, "the arena match did not start");
        }
    }
}

/// A smoke test that always fails after [`SMOKE_FRAMES`] (`arena --smoke-fail`): it proves a failing
/// session saves a failure screenshot and names it (`smoke-fail.session.ron`, ADR-2755).
#[derive(Default)]
pub struct ArenaSmokeFail;

impl TestBody for ArenaSmokeFail {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.begin_step("fails on purpose");
    }

    fn tick(&mut self, cx: &mut TestContext) {
        if cx.frame() >= SMOKE_FRAMES {
            cx.is_true(false, "this test fails on purpose");
            cx.end_step();
            cx.conclude(Verdict::Failed, "failing on purpose");
        }
    }
}

/// Frames a headless CTF bot match may take before the test fails (the arena's match timer ends a
/// match well inside it).
pub const CTF_FRAME_LIMIT: u32 = 60 * 200;

/// The CTF functional test (`arena_ctf_bots`, tag `ctf`): two bots per side play the shipped `ctf`
/// rule set through the hosted session until it ends. Passes when the session reached post-match
/// on its capture limit, with every flag counted by the board, and the arena's view agrees.
#[derive(Default)]
pub struct ArenaCtfBots;

impl TestBody for ArenaCtfBots {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.begin_step("bots play capture the flag to the end");
    }

    fn tick(&mut self, cx: &mut TestContext) {
        use regia_framework::rules::MatchRules;
        use regia_framework::rules::mode::MatchMode;
        use regia_framework::rules::objective::EndReason;
        use regia_framework::rules::phases::MatchPhase;

        let Some((mode, phase, over, reason, scores, limit)) =
            cx.world().get_resource::<MatchRules>().map(|rules| {
                let session = &rules.session;
                (
                    session.board.mode,
                    session.flow.phases.phase,
                    session.board.match_over,
                    session.board.end_reason,
                    session.board.team_scores.clone(),
                    session.board.settings.effective_capture_limit(),
                )
            })
        else {
            return;
        };
        // The board ends the match; the flow then moves to post-match on the same step.
        if !over || phase != MatchPhase::PostMatch {
            return;
        }
        cx.equal(
            &mode,
            &MatchMode::CaptureTheFlag,
            "the session plays the ctf rule set",
        );
        cx.equal(
            &reason,
            &Some(EndReason::CaptureLimitReached),
            "a capture ended it",
        );
        cx.is_true(
            scores.iter().any(|&s| s >= limit),
            "a team reached the capture limit",
        );
        cx.end_step();
        if cx.failure_count() == 0 {
            cx.conclude(Verdict::Passed, "the bots captured to the limit");
        } else {
            cx.conclude(Verdict::Failed, "the ctf match ended the wrong way");
        }
    }
}

/// `Startup`: places the CTF test.
fn spawn_ctf_test(mut commands: Commands) {
    commands.spawn((
        Name::new("arena_ctf_bots"),
        FunctionalTest {
            description: "two bots per side play ctf through the match core to the end".into(),
            owner: "arena".into(),
            tags: vec!["ctf".into()],
            frame_limit: CTF_FRAME_LIMIT,
            ready_limit: 600,
            body: TestBodyRef::Rust(std::any::type_name::<ArenaCtfBots>().into()),
            ..FunctionalTest::default()
        },
    ));
}

/// Installs the runner, the CTF body and its placed test. The app must run a CTF mode
/// (`crate::modes::ArenaMatchMode`).
pub fn install_ctf(app: &mut App) {
    app.add_plugins(FunctionalTestPlugin)
        .register_functional_test::<ArenaCtfBots>()
        .add_systems(Startup, spawn_ctf_test);
}

/// `Startup`: places the smoke test.
fn spawn_smoke_test(mut commands: Commands) {
    commands.spawn((
        Name::new("arena_smoke"),
        FunctionalTest {
            description: "the arena match starts and runs".into(),
            owner: "arena".into(),
            tags: vec!["smoke".into()],
            frame_limit: 600,
            ready_limit: 600,
            body: TestBodyRef::Rust(std::any::type_name::<ArenaSmoke>().into()),
            ..FunctionalTest::default()
        },
    ));
}

/// `Startup`: places the test that fails on purpose.
fn spawn_smoke_fail_test(mut commands: Commands) {
    commands.spawn((
        Name::new("arena_smoke_fail"),
        FunctionalTest {
            description: "fails on purpose, to prove failure screenshots".into(),
            owner: "arena".into(),
            tags: vec!["smoke_fail".into()],
            frame_limit: 600,
            ready_limit: 600,
            body: TestBodyRef::Rust(std::any::type_name::<ArenaSmokeFail>().into()),
            ..FunctionalTest::default()
        },
    ));
}

/// Installs the runner, the body and the placed test.
pub fn install(app: &mut App) {
    app.add_plugins(FunctionalTestPlugin)
        .register_functional_test::<ArenaSmoke>()
        .add_systems(Startup, spawn_smoke_test);
}

/// Installs the runner and the test that fails on purpose (`--smoke-fail`).
pub fn install_failing(app: &mut App) {
    app.add_plugins(FunctionalTestPlugin)
        .register_functional_test::<ArenaSmokeFail>()
        .add_systems(Startup, spawn_smoke_fail_test);
}
