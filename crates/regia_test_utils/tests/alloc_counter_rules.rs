// SPDX-License-Identifier: MPL-2.0
//! The rules of `regia_test_utils::alloc_counter` against the three causes of the allocation
//! tests' random failures (ADR-5718), each proved on a model of the cause with the counting
//! allocator installed: work that moves between threads, tests side by side, and the build's
//! `tracing` feature set. Plus the tracer that names the site of an allocation.
//!
//! Its own test binary: it installs the counting global allocator. Counts are taken on the
//! calling thread or on threads this file starts, so the tests can run beside each other; the two
//! that use the counted-thread total take `exclusive` first.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use regia_ecs::ecs::schedule::{Schedule, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck, SteadyAllocError};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Makes `n` heap requests and frees them.
#[inline(never)]
fn a_segment_s_work(n: usize) {
    for i in 0..n {
        drop(std::hint::black_box(Box::new([i as u64; 4])));
    }
}

/// A model of the renderer's encode worker: a persistent thread that does a segment's work when
/// asked, with a handshake that allocates nothing (a mutex and a condition variable, as
/// `parallel_finish` uses).
struct Worker {
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    Idle,
    Work(usize),
    Done,
    Exit,
}

impl Worker {
    fn start() -> Self {
        let state = Arc::new((Mutex::new(WorkerState::Idle), Condvar::new()));
        let shared = Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            // The start hook of the model: the worker's requests count with the frame's.
            alloc_counter::count_this_thread();
            let (lock, ready) = &*shared;
            let mut state = lock.lock().expect("worker state");
            loop {
                match *state {
                    WorkerState::Work(n) => {
                        a_segment_s_work(n);
                        *state = WorkerState::Done;
                        ready.notify_all();
                    }
                    WorkerState::Exit => return,
                    WorkerState::Idle | WorkerState::Done => {
                        state = ready.wait(state).expect("worker state");
                    }
                }
            }
        });
        let worker = Self {
            state,
            thread: Some(thread),
        };
        // One empty handshake before any count: where std's mutex and condition variable are
        // pthread objects (macOS), each allocates its state on first use, once, on whichever
        // thread touches it first (3 extra requests in the M5 run's first traced segment,
        // ADR-7484). After this the handshake allocates nothing on every host, as the model needs.
        worker.run(0);
        worker
    }

    /// Has the worker do `n` requests and waits for it.
    fn run(&self, n: usize) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().expect("worker state");
        *state = WorkerState::Work(n);
        ready.notify_all();
        while *state != WorkerState::Done {
            state = ready.wait(state).expect("worker state");
        }
        *state = WorkerState::Idle;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let (lock, ready) = &*self.state;
            *lock.lock().expect("worker state") = WorkerState::Exit;
            ready.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A frame of the model: 200 requests on the drawing thread and a 48-request segment that a
/// worker finishes, or the drawing thread itself on the frames listed in `stolen` (the worker was
/// late: `FinishWorkers::join`, ADR-2299). `growing` adds one request per frame drawn so far: a
/// pool that grows every frame.
struct Frames {
    worker: Worker,
    drawn: u32,
    stolen: Vec<u32>,
    growing: bool,
}

impl Frames {
    fn draw(&mut self) {
        a_segment_s_work(200);
        if self.growing {
            a_segment_s_work(self.drawn as usize);
        }
        if self.stolen.contains(&self.drawn) {
            a_segment_s_work(48);
        } else {
            self.worker.run(48);
        }
        self.drawn += 1;
    }
}

const RULE: SteadyAllocCheck = SteadyAllocCheck::frames(8);

/// Cause 1, work that moves between threads. The same frames, some with the segment finished on
/// the drawing thread: the drawing thread's own count jumps by the segment and fails the rule (the
/// old lone "+48" batch); the counted threads' total is the same every batch and passes. A frame
/// that really grows still fails on the total.
#[test]
fn a_stolen_segment_moves_the_drawing_thread_s_count_and_not_the_frame_s() {
    let _alone = alloc_counter::exclusive();
    alloc_counter::count_this_thread();
    // Warm 40 frames, settle in 2 batches (frames 40..56), then measure 6: frame 70 is in the
    // second measured batch.
    let stolen = vec![70];

    let mut frames = Frames {
        worker: Worker::start(),
        drawn: 0,
        stolen: stolen.clone(),
        growing: false,
    };
    let alone = RULE.run(&mut frames, alloc_counter::thread_allocs, Frames::draw, |_| false);
    let Err(SteadyAllocError::Grows { run, .. }) = alone else {
        panic!("the drawing thread alone must see the stolen segment: {alone:?}");
    };
    assert_eq!(run.steady, 8 * 200);
    assert_eq!(run.measured[1], 8 * 200 + 48, "{run:?}");

    let mut frames = Frames {
        worker: Worker::start(),
        drawn: 0,
        stolen,
        growing: false,
    };
    let together = RULE
        .run(&mut frames, alloc_counter::counted_allocs, Frames::draw, |_| {
            false
        })
        .expect("the frame's threads together do the same work every batch");
    assert_eq!(together.steady, 8 * 248);
    assert_eq!(together.measured, vec![8 * 248; 6]);

    // A real steady-state allocation still fails: one more request every frame.
    let mut frames = Frames {
        worker: Worker::start(),
        drawn: 0,
        stolen: Vec::new(),
        growing: true,
    };
    let grows = RULE.run(&mut frames, alloc_counter::counted_allocs, Frames::draw, |_| {
        false
    });
    assert!(
        matches!(grows, Err(SteadyAllocError::NeverSettled { .. })),
        "a frame that allocates more every frame never settles: {grows:?}"
    );
    alloc_counter::stop_counting_this_thread();
}

/// A step up after settling (a cache that starts to miss) fails on the total too, and the lone
/// batch the rule used to forgive is not forgiven.
#[test]
fn a_step_after_settling_fails_on_the_counted_total() {
    struct Step {
        frames: Frames,
        from: u32,
    }
    let _alone = alloc_counter::exclusive();
    alloc_counter::count_this_thread();
    let mut step = Step {
        frames: Frames {
            worker: Worker::start(),
            drawn: 0,
            stolen: Vec::new(),
            growing: false,
        },
        from: 80,
    };
    let result = RULE.run(
        &mut step,
        alloc_counter::counted_allocs,
        |s| {
            if s.frames.drawn >= s.from {
                // Five more requests per frame from frame 80 on: 40 per batch, over the 32 slack.
                a_segment_s_work(5);
            }
            s.frames.draw();
        },
        |_| false,
    );
    let Err(SteadyAllocError::Grows { run, reason, .. }) = result else {
        panic!("a step up must fail: {result:?}");
    };
    assert_eq!(run.measured[3..], [8 * 253; 3], "{run:?}");
    assert!(reason.contains("[3, 4, 5]"), "{reason}");
    alloc_counter::stop_counting_this_thread();
}

/// Cause 2, tests side by side: `exclusive` holders never overlap.
#[test]
fn exclusive_runs_one_holder_at_a_time() {
    static INSIDE: AtomicU32 = AtomicU32::new(0);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..200 {
                    let _alone = alloc_counter::exclusive();
                    assert_eq!(INSIDE.fetch_add(1, Ordering::SeqCst), 0, "two holders at once");
                    std::thread::yield_now();
                    INSIDE.fetch_sub(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("holder");
    }
}

fn nothing() {}

/// Cause 3, the build's feature set: a system that does nothing, run on this thread, allocates
/// nothing per run. Without `quiet_tracing` (which `measure` calls) a whole-workspace build counts
/// one `String` per run here, for a disabled span, and a build of this package alone counts none.
#[test]
fn a_schedule_run_allocates_nothing_for_a_disabled_span() {
    let mut world = World::new();
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    schedule.add_systems((nothing, nothing_else, nothing));
    schedule.run(&mut world);
    schedule.run(&mut world);
    let ((), counted) = alloc_counter::measure(|| {
        for _ in 0..10 {
            schedule.run(&mut world);
        }
    });
    assert_eq!(
        counted.allocs, 0,
        "10 runs of three systems that do nothing allocated {} times",
        counted.allocs
    );
}

fn nothing_else() {}

#[inline(never)]
fn a_marked_site() -> Box<[u8; 48]> {
    std::hint::black_box(Box::new([7; 48]))
}

#[inline(never)]
fn another_marked_site() -> Vec<u32> {
    std::hint::black_box(Vec::with_capacity(9))
}

/// The tracer names the function behind a request, and its own requests are not counted.
#[test]
fn a_trace_names_the_function_that_allocated() {
    let ((kept, trace), counted) = alloc_counter::measure(|| alloc_counter::trace(a_marked_site));
    assert_eq!(kept[0], 7);
    assert_eq!(counted.allocs, 1, "the tracer's own requests must not count");
    assert_eq!(trace.len(), 1);
    let sites = trace.sites();
    assert_eq!(sites.len(), 1);
    assert_eq!(
        (sites[0].requests, sites[0].bytes, sites[0].on_tracing_thread),
        (1, 48, 1)
    );
    assert_eq!(sites[0].reallocs, 0, "a new block, not a container that grew");
    assert!(
        sites[0].stack.iter().any(|frame| frame.contains("a_marked_site")),
        "the stack does not name the allocating function:\n{}",
        trace.report(4)
    );
    // The allocator's own frames are cut from the front, the harness's from the back.
    assert!(
        !sites[0].stack.iter().any(|frame| frame.contains("trace_request")),
        "{}",
        trace.report(4)
    );
    assert!(trace.report(4).contains("1 requests traced"));
}

/// One part of the traced work: three requests at one site, and two at another when `more`.
#[inline(never)]
fn a_part(more: bool) {
    for _ in 0..3 {
        drop(a_marked_site());
    }
    if more {
        for _ in 0..2 {
            drop(another_marked_site());
        }
    }
}

/// Two marked parts of one trace, the same code with more to do the second time: `grown` lists
/// what the second did that the first did not.
#[test]
fn grown_sites_are_what_a_later_part_added() {
    // Another thread relabels as fast as it can for the whole trace, as a neighbour test would.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let meddler = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut mark = 100;
            while !stop.load(Ordering::Relaxed) {
                alloc_counter::set_trace_mark(mark);
                mark += 1;
                std::thread::yield_now();
            }
        })
    };
    let ((), trace) = alloc_counter::trace(|| {
        for mark in 0..2 {
            alloc_counter::set_trace_mark(mark);
            a_part(mark == 1);
        }
    });
    stop.store(true, Ordering::Relaxed);
    meddler.join().expect("meddler");
    // A test beside this one (an untraced steady run) sets marks all the time; they must not
    // reach this trace. Failed 3 loaded runs of 20 when the mark was anyone's to set.
    assert_eq!((trace.len_at(0), trace.len_at(1)), (3, 5));
    let grown = trace.grown(0, 1);
    assert_eq!(grown.len(), 1, "{}", alloc_counter::sites_text(&grown, 8));
    assert_eq!(grown[0].requests, 2);
    assert!(
        grown[0]
            .stack
            .iter()
            .any(|frame| frame.contains("another_marked_site")),
        "{}",
        alloc_counter::sites_text(&grown, 8)
    );
    assert!(trace.grown(1, 0).is_empty(), "the first part added nothing");
}

/// Work that a counted worker does in one part and the tracing thread in the next is growth by
/// whole stack (the threads' callers differ) and none by site: `grown_at` cancels what only
/// moved, so a batch with a stolen segment and no new allocation lists nothing.
#[test]
fn work_that_moved_between_threads_cancels_out_by_site() {
    let _alone = alloc_counter::exclusive();
    let worker = Worker::start();
    let ((), trace) = alloc_counter::trace_counted(|| {
        alloc_counter::set_trace_mark(0);
        worker.run(48);
        alloc_counter::set_trace_mark(1);
        a_segment_s_work(48);
    });
    assert_eq!((trace.len_at(0), trace.len_at(1)), (48, 48));
    let by_stack: u64 = trace.grown(0, 1).iter().map(|site| site.requests).sum();
    assert_eq!(by_stack, 48, "by whole stack the tracing thread's 48 are new");
    let by_site = trace.grown_at(0, 1, 3);
    assert!(
        by_site.is_empty(),
        "the same 48 requests at the same site moved, none was added:\n{}",
        alloc_counter::sites_text(&by_site, 8)
    );
}

/// A counted worker's requests are traced with the tracing thread's by `trace_counted`, and
/// attributed to it.
#[test]
fn a_counted_worker_is_traced_with_the_frame() {
    let _alone = alloc_counter::exclusive();
    let worker = Worker::start();
    let ((), trace) = alloc_counter::trace_counted(|| {
        drop(a_marked_site());
        worker.run(5);
    });
    let sites = trace.sites();
    let on_worker: u64 = sites.iter().map(|s| s.requests - s.on_tracing_thread).sum();
    let here: u64 = sites.iter().map(|s| s.on_tracing_thread).sum();
    assert_eq!((here, on_worker), (1, 5), "{}", trace.report(8));
}

/// A traced run lists each batch that differs from the usual one with the sites behind the
/// difference, in both directions, also when the run passes: an event inside the slack (a pool
/// that grew once, a request that comes and goes with the machine's load) is named without
/// making the test fail first.
#[test]
fn a_traced_run_names_the_sites_of_each_batch_that_differs() {
    let _alone = alloc_counter::exclusive();
    let rule = SteadyAllocCheck::frames(4);
    // 40 warm frames, then batches of 4: frames 41 to 48 settle, 49 to 72 are measured. Frame 55
    // (batch 3) makes two requests more at another site, frame 62 (batch 5) one fewer at the usual
    // one.
    let draw = |frame: &mut u32| {
        *frame += 1;
        let usual = if *frame == 62 { 2 } else { 3 };
        for _ in 0..usual {
            drop(a_marked_site());
        }
        if *frame == 55 {
            for _ in 0..2 {
                drop(another_marked_site());
            }
        }
    };
    let mut frame = 0u32;
    let run = rule
        .run_traced(&mut frame, alloc_counter::thread_allocs, draw, |_| false, 8)
        .expect("both events are inside the slack");
    assert_eq!(run.settling, [12, 12]);
    assert_eq!(run.measured, [12, 14, 12, 11, 12, 12]);
    let text = &run.deviations;
    let mut blocks = text.split("batch 5 counted");
    let (grew, shrank) = (blocks.next().unwrap_or(""), blocks.next().unwrap_or(""));
    assert!(
        grew.contains("batch 3 counted 14, the usual batch (batch 0) 12"),
        "{text}"
    );
    assert!(grew.contains("sites with more requests (2 in all"), "{text}");
    assert!(grew.contains("another_marked_site"), "{text}");
    assert!(grew.contains("sites with fewer (0 in all)"), "{text}");
    assert!(shrank.starts_with(" 11, the usual batch (batch 0) 12"), "{text}");
    assert!(shrank.contains("sites with more requests (0 in all"), "{text}");
    assert!(shrank.contains("sites with fewer (1 in all)"), "{text}");
    assert!(shrank.contains("a_marked_site"), "{text}");
    assert!(run.to_string().contains("batches that differ from the usual one"));

    // A run in which every batch counts the same lists nothing, traced or not.
    let mut frame = 0u32;
    let same = rule
        .run_traced(
            &mut frame,
            alloc_counter::thread_allocs,
            |_| drop(a_marked_site()),
            |_| false,
            8,
        )
        .expect("steady");
    assert!(same.deviations.is_empty(), "{}", same.deviations);
}
