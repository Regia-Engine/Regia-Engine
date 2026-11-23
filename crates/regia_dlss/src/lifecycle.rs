// SPDX-License-Identifier: MPL-2.0
//! One shared value per key, opened once and closed once (audit 11-03, dlss M1 and M3).
//!
//! NGX is initialised once per device and shut down once. The registry that guaranteed this
//! lived in the renderer's seam, so two direct [`crate::Ngx`] instances on one device each shut
//! NGX down under the other; and the seam held one process-wide lock across NGX's native init and
//! across the GPU waits of a teardown, so a slow device stalled every other device's renderer.
//!
//! [`Lifecycle`] keeps one slot per key. A slot is `Opening` while its value is being made (no
//! lock is held while `open` runs) and `Open` with a weak reference afterwards. A caller for a
//! key that is opening, or whose last owner is dropping its value (a dead weak reference), waits
//! for that key only; other keys go on. The value's `Drop` calls [`Lifecycle::closed`] after it
//! released its resources, which forgets the slot and wakes the waiters.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};

/// A key's state.
enum Slot<T> {
    /// `open` is running for the key.
    Opening,
    /// The value; dead while its last owner drops it, until [`Lifecycle::closed`].
    Open(Weak<T>),
}

struct Entry<K, T> {
    key: K,
    slot: Slot<T>,
}

/// One shared `T` per key `K` (see the module docs).
pub(crate) struct Lifecycle<K, T> {
    entries: Mutex<Vec<Entry<K, T>>>,
    changed: Condvar,
    /// Threads waiting for a key right now (tests watch it to know a caller is blocked).
    #[cfg(test)]
    waiting: std::sync::atomic::AtomicUsize,
}

impl<K: PartialEq + Clone, T> Lifecycle<K, T> {
    /// An empty registry (usable in a `static`).
    pub(crate) const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            changed: Condvar::new(),
            #[cfg(test)]
            waiting: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Entry<K, T>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, entries: MutexGuard<'a, Vec<Entry<K, T>>>) -> MutexGuard<'a, Vec<Entry<K, T>>> {
        #[cfg(test)]
        self.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let entries = self.changed.wait(entries).unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        self.waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        entries
    }

    /// The live value of `key`, or a new one from `open`, which runs with no lock held. Waits
    /// while `key` is opening on another thread or its value is closing. An error or a panic of
    /// `open` frees the key for the next caller.
    pub(crate) fn get_or_open<E>(
        &self,
        key: &K,
        open: impl FnOnce() -> Result<Arc<T>, E>,
    ) -> Result<Arc<T>, E> {
        let mut entries = self.lock();
        loop {
            let found = entries
                .iter()
                .find(|entry| entry.key == *key)
                .map(|entry| match &entry.slot {
                    Slot::Opening => None,
                    Slot::Open(weak) => weak.upgrade(),
                });
            match found {
                None => break,
                Some(Some(live)) => return Ok(live),
                // Opening, or the last owner is closing it: wait for this key only.
                Some(None) => entries = self.wait(entries),
            }
        }
        entries.push(Entry {
            key: key.clone(),
            slot: Slot::Opening,
        });
        drop(entries);
        let _opening = OpeningGuard { lifecycle: self, key };
        let value = open()?;
        let mut entries = self.lock();
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.key == *key && matches!(entry.slot, Slot::Opening))
        {
            entry.slot = Slot::Open(Arc::downgrade(&value));
        }
        Ok(value)
    }

    /// Forgets the slot of `value` and wakes the callers waiting for its key. `T`'s `Drop` calls
    /// this once it released what it holds (so a reopen of the key starts after that).
    pub(crate) fn closed(&self, value: *const T) {
        let mut entries = self.lock();
        entries
            .retain(|entry| !matches!(&entry.slot, Slot::Open(weak) if std::ptr::eq(weak.as_ptr(), value)));
        drop(entries);
        self.changed.notify_all();
    }
}

/// Removes a key's `Opening` slot when `open` returns (its value is `Open` by then on success)
/// or fails or panics, and wakes the waiters.
struct OpeningGuard<'a, K: PartialEq + Clone, T> {
    lifecycle: &'a Lifecycle<K, T>,
    key: &'a K,
}

impl<K: PartialEq + Clone, T> Drop for OpeningGuard<'_, K, T> {
    fn drop(&mut self) {
        let mut entries = self.lifecycle.lock();
        entries.retain(|entry| !(entry.key == *self.key && matches!(entry.slot, Slot::Opening)));
        drop(entries);
        self.lifecycle.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, mpsc};

    /// A value whose drop blocks until the test releases it (a GPU wait before NGX's shutdown),
    /// then logs and reports itself closed.
    struct Probe {
        lifecycle: Arc<Lifecycle<u32, Probe>>,
        log: Arc<Mutex<Vec<&'static str>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            if let Some(release) = self.release.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = release.recv();
            }
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push("closed");
            self.lifecycle.closed(self);
        }
    }

    fn probe(
        lifecycle: &Arc<Lifecycle<u32, Probe>>,
        log: &Arc<Mutex<Vec<&'static str>>>,
        release: Option<mpsc::Receiver<()>>,
    ) -> Arc<Probe> {
        Arc::new(Probe {
            lifecycle: Arc::clone(lifecycle),
            log: Arc::clone(log),
            release: Mutex::new(release),
        })
    }

    /// Spins (yielding) until `n` callers wait in the registry: a counter, not a clock.
    fn until_waiting(lifecycle: &Lifecycle<u32, Probe>, n: usize) {
        while lifecycle.waiting.load(Ordering::SeqCst) < n {
            std::thread::yield_now();
        }
    }

    /// M1: two callers for one key share one value; `open` runs once.
    #[test]
    fn a_key_opens_once_and_is_shared() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let opens = AtomicUsize::new(0);
        let open = || {
            opens.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(probe(&lifecycle, &log, None))
        };
        let a = lifecycle.get_or_open(&7, open).expect("open");
        let b = lifecycle
            .get_or_open(&7, || {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(probe(&lifecycle, &log, None))
            })
            .expect("shared");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        drop((a, b));
        assert_eq!(*log.lock().expect("log"), ["closed"]);
        assert!(
            lifecycle.lock().is_empty(),
            "the closed value's slot is forgotten"
        );
    }

    /// M1: a reopen of a key whose value is still closing waits until the close finished (NGX's
    /// shutdown never runs after a new init on the same device).
    #[test]
    fn a_reopen_waits_for_the_close_of_the_same_key() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (release, blocked) = mpsc::channel();
        let first = lifecycle
            .get_or_open(&1, || Ok::<_, ()>(probe(&lifecycle, &log, Some(blocked))))
            .expect("open");
        let closer = std::thread::spawn(move || drop(first));
        // The closer is inside the drop (blocked on `release`) once the slot's value is dead.
        while lifecycle
            .lock()
            .iter()
            .any(|entry| matches!(&entry.slot, Slot::Open(weak) if weak.strong_count() > 0))
        {
            std::thread::yield_now();
        }
        let reopener = {
            let lifecycle = Arc::clone(&lifecycle);
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let value = lifecycle
                    .get_or_open(&1, || {
                        log.lock().unwrap_or_else(PoisonError::into_inner).push("opened");
                        Ok::<_, ()>(probe(&lifecycle, &log, None))
                    })
                    .expect("reopen");
                drop(value);
            })
        };
        until_waiting(&lifecycle, 1);
        release.send(()).expect("release the close");
        closer.join().expect("closer");
        reopener.join().expect("reopener");
        assert_eq!(*log.lock().expect("log"), ["closed", "opened", "closed"]);
    }

    /// M3: a key whose value is closing (a long GPU wait) does not hold up another key.
    #[test]
    fn another_key_is_not_held_up_by_a_closing_key() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (release, blocked) = mpsc::channel();
        let slow = lifecycle
            .get_or_open(&1, || Ok::<_, ()>(probe(&lifecycle, &log, Some(blocked))))
            .expect("open");
        let closer = std::thread::spawn(move || drop(slow));
        while lifecycle
            .lock()
            .iter()
            .any(|entry| matches!(&entry.slot, Slot::Open(weak) if weak.strong_count() > 0))
        {
            std::thread::yield_now();
        }
        // Key 2 opens and closes while key 1 is still inside its drop.
        let other = lifecycle
            .get_or_open(&2, || Ok::<_, ()>(probe(&lifecycle, &log, None)))
            .expect("another key");
        drop(other);
        assert_eq!(
            *log.lock().expect("log"),
            ["closed"],
            "key 2 closed; key 1 still closing"
        );
        release.send(()).expect("release");
        closer.join().expect("closer");
        assert_eq!(log.lock().expect("log").len(), 2);
    }

    /// A failed or panicking `open` frees the key: the next caller opens it.
    #[test]
    fn a_failed_or_panicking_open_frees_the_key() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(
            lifecycle
                .get_or_open(&3, || Err::<Arc<Probe>, _>("no runtime"))
                .err(),
            Some("no runtime")
        );
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = lifecycle.get_or_open(&3, || -> Result<Arc<Probe>, ()> { panic!("native init") });
        }));
        assert!(panicked.is_err());
        let value = lifecycle
            .get_or_open(&3, || Ok::<_, ()>(probe(&lifecycle, &log, None)))
            .expect("the key is free again");
        drop(value);
        assert!(lifecycle.lock().is_empty());
    }

    /// A failed opener wakes a whole cohort; exactly one waiter becomes the next opener while
    /// the others continue waiting on that key. An unrelated live key remains independent.
    fn failed_generation_with_waiters(caller_count: usize, panic_open: bool) {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let other_opens = AtomicUsize::new(0);
        let other = lifecycle
            .get_or_open(&99, || {
                other_opens.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(probe(&lifecycle, &log, None))
            })
            .expect("other key opens");

        let (opening_tx, opening_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let opener_lifecycle = Arc::clone(&lifecycle);
        let opener = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                opener_lifecycle.get_or_open(&7, || {
                    opening_tx.send(()).expect("signal first opening");
                    release_rx
                        .lock()
                        .expect("release lock")
                        .recv()
                        .expect("release first opening");
                    if panic_open {
                        panic!("deliberate opening panic");
                    }
                    Err::<Arc<Probe>, _>("deliberate opening failure")
                })
            }))
        });
        opening_rx.recv().expect("first key is opening");

        let (ready_tx, ready_rx) = mpsc::channel();
        let (success_open_tx, success_open_rx) = mpsc::channel();
        let (success_release_tx, success_release_rx) = mpsc::channel();
        let success_release_rx = Arc::new(Mutex::new(success_release_rx));
        let (results_tx, results_rx) = mpsc::channel();
        let mut callers = Vec::new();
        for _ in 0..caller_count {
            let lifecycle = Arc::clone(&lifecycle);
            let log = Arc::clone(&log);
            let ready_tx = ready_tx.clone();
            let success_open_tx = success_open_tx.clone();
            let success_release_rx = Arc::clone(&success_release_rx);
            let results_tx = results_tx.clone();
            callers.push(std::thread::spawn(move || {
                ready_tx.send(()).expect("report caller ready");
                let value = lifecycle
                    .get_or_open(&7, || {
                        success_open_tx.send(()).expect("signal successor opening");
                        success_release_rx
                            .lock()
                            .expect("success release lock")
                            .recv()
                            .expect("release successor");
                        Ok::<_, ()>(probe(&lifecycle, &log, None))
                    })
                    .expect("successor opens");
                results_tx.send(value).expect("return shared value");
            }));
        }
        drop(ready_tx);
        drop(success_open_tx);
        drop(results_tx);
        for _ in 0..caller_count {
            ready_rx.recv().expect("caller started");
        }
        until_waiting(&lifecycle, caller_count);

        // While key 7 is opening, key 99 still returns its existing value without reopening.
        let same_other = lifecycle
            .get_or_open(&99, || {
                other_opens.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(probe(&lifecycle, &log, None))
            })
            .expect("other key remains available");
        assert!(Arc::ptr_eq(&other, &same_other));
        assert_eq!(other_opens.load(Ordering::SeqCst), 1);

        release_tx.send(()).expect("fail the first generation");
        if panic_open {
            assert!(opener.join().expect("opener thread").is_err());
        } else {
            assert_eq!(
                opener.join().expect("opener thread").expect("no panic").err(),
                Some("deliberate opening failure")
            );
        }

        success_open_rx.recv().expect("one successor starts");
        until_waiting(&lifecycle, caller_count - 1);
        assert!(
            success_open_rx.try_recv().is_err(),
            "exactly one waiter owns the successor opening"
        );
        assert_eq!(
            lifecycle.lock().iter().filter(|entry| entry.key == 7).count(),
            1,
            "failed opening left no stale slot"
        );
        success_release_tx.send(()).expect("complete successor");

        let values: Vec<_> = (0..caller_count)
            .map(|_| results_rx.recv().expect("caller receives shared value"))
            .collect();
        for value in &values[1..] {
            assert!(Arc::ptr_eq(&values[0], value));
        }
        assert_eq!(lifecycle.lock().iter().filter(|entry| entry.key == 7).count(), 1);
        drop(values);
        drop((same_other, other));
        for caller in callers {
            caller.join().expect("caller thread");
        }
        assert!(lifecycle.lock().is_empty());
        assert_eq!(*log.lock().expect("close log"), ["closed", "closed"]);
    }

    #[test]
    fn failed_open_wakes_multiple_waiters_and_one_successor_opens() {
        for caller_count in [2, 5, 8] {
            failed_generation_with_waiters(caller_count, false);
        }
    }

    #[test]
    fn panicking_open_wakes_multiple_waiters_and_one_successor_opens() {
        for caller_count in [3, 6, 9] {
            failed_generation_with_waiters(caller_count, true);
        }
    }

    #[test]
    fn two_failed_generations_leave_no_stale_slot_before_success() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (start_tx, start_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        // Callers enter `get_or_open` only after the starter is inside the first opening, so the
        // starter owns generation 0 (the six callers plus this thread).
        let gate = Arc::new(Barrier::new(7));
        let starter_lifecycle = Arc::clone(&lifecycle);
        let starter_start_tx = start_tx.clone();
        let starter_release_rx = Arc::clone(&release_rx);
        let starter = std::thread::spawn(move || {
            starter_lifecycle.get_or_open(&8, || {
                starter_start_tx.send(()).expect("first generation starts");
                starter_release_rx
                    .lock()
                    .expect("first generation release lock")
                    .recv()
                    .expect("release first generation");
                Err::<Arc<Probe>, _>("first opening failed")
            })
        });
        let mut callers = Vec::new();
        for _ in 0..6 {
            let lifecycle = Arc::clone(&lifecycle);
            let log = Arc::clone(&log);
            let start_tx = start_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            let ready_tx = ready_tx.clone();
            let result_tx = result_tx.clone();
            let gate = Arc::clone(&gate);
            callers.push(std::thread::spawn(move || {
                ready_tx.send(()).expect("ready");
                gate.wait();
                let value = lifecycle.get_or_open(&8, || {
                    start_tx.send(()).expect("opening generation started");
                    let generation = release_rx
                        .lock()
                        .expect("generation release lock")
                        .recv()
                        .expect("opening generation released");
                    if generation < 2 {
                        Err::<Arc<Probe>, _>("opening generation failed")
                    } else {
                        Ok(probe(&lifecycle, &log, None))
                    }
                });
                result_tx.send(value).expect("return generation result");
            }));
        }
        drop(start_tx);
        drop(ready_tx);
        drop(result_tx);
        for _ in 0..6 {
            ready_rx.recv().expect("caller ready");
        }
        start_rx.recv().expect("first generation is opening");
        gate.wait();
        until_waiting(&lifecycle, 6);

        release_tx.send(0).expect("fail first generation");
        assert_eq!(
            starter.join().expect("starter thread").err(),
            Some("first opening failed")
        );
        start_rx.recv().expect("second generation starts");
        until_waiting(&lifecycle, 5);
        assert!(start_rx.try_recv().is_err(), "one opener in second generation");
        release_tx.send(1).expect("fail second generation");
        start_rx.recv().expect("third generation starts");
        until_waiting(&lifecycle, 4);
        assert!(start_rx.try_recv().is_err(), "one opener in third generation");
        let entries = lifecycle.lock();
        assert_eq!(entries.iter().filter(|entry| entry.key == 8).count(), 1);
        assert!(matches!(&entries[0].slot, Slot::Opening));
        drop(entries);
        release_tx.send(2).expect("complete successful generation");
        let results: Vec<_> = (0..6)
            .map(|_| result_rx.recv().expect("caller gets generation result"))
            .collect();
        let errors: Vec<_> = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect();
        assert_eq!(errors, [&"opening generation failed"]);
        let values: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        assert_eq!(values.len(), 5);
        for value in &values[1..] {
            assert!(Arc::ptr_eq(&values[0], value));
        }
        drop(values);
        for caller in callers {
            caller.join().expect("caller thread");
        }
        assert!(lifecycle.lock().is_empty());
        assert_eq!(*log.lock().expect("close log"), ["closed"]);
    }

    #[test]
    fn close_reopen_handoff_serializes_three_waiters_and_closes_each_value_once() {
        let lifecycle = Arc::new(Lifecycle::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (release_close_tx, release_close_rx) = mpsc::channel();
        let first = lifecycle
            .get_or_open(&11, || {
                Ok::<_, ()>(probe(&lifecycle, &log, Some(release_close_rx)))
            })
            .expect("first generation");
        let closer = std::thread::spawn(move || drop(first));
        while lifecycle
            .lock()
            .iter()
            .any(|entry| matches!(&entry.slot, Slot::Open(weak) if weak.strong_count() > 0))
        {
            std::thread::yield_now();
        }

        let (ready_tx, ready_rx) = mpsc::channel();
        let (opened_tx, opened_rx) = mpsc::channel();
        let (release_open_tx, release_open_rx) = mpsc::channel();
        let release_open_rx = Arc::new(Mutex::new(release_open_rx));
        let (values_tx, values_rx) = mpsc::channel();
        let (hold_tx, hold_rx) = mpsc::channel();
        let hold_rx = Arc::new(Mutex::new(hold_rx));
        let mut callers = Vec::new();
        for _ in 0..3 {
            let lifecycle = Arc::clone(&lifecycle);
            let log = Arc::clone(&log);
            let ready_tx = ready_tx.clone();
            let opened_tx = opened_tx.clone();
            let release_open_rx = Arc::clone(&release_open_rx);
            let hold_rx = Arc::clone(&hold_rx);
            let values_tx = values_tx.clone();
            callers.push(std::thread::spawn(move || {
                ready_tx.send(()).expect("ready");
                let value = lifecycle
                    .get_or_open(&11, || {
                        opened_tx.send(()).expect("reopen begins");
                        release_open_rx
                            .lock()
                            .expect("reopen release lock")
                            .recv()
                            .expect("release reopened value");
                        Ok::<_, ()>(probe(&lifecycle, &log, None))
                    })
                    .expect("reopen succeeds");
                values_tx.send(value).expect("return reopened value");
                hold_rx
                    .lock()
                    .expect("hold lock")
                    .recv()
                    .expect("hold shared value alive");
            }));
        }
        drop(ready_tx);
        drop(opened_tx);
        drop(values_tx);
        for _ in 0..3 {
            ready_rx.recv().expect("caller ready");
        }
        until_waiting(&lifecycle, 3);
        release_close_tx.send(()).expect("finish close");
        closer.join().expect("old value closes");
        opened_rx.recv().expect("one reopen begins");
        until_waiting(&lifecycle, 2);
        assert!(opened_rx.try_recv().is_err(), "only one waiter reopens");
        release_open_tx.send(()).expect("complete reopened value");
        let values: Vec<_> = (0..3)
            .map(|_| values_rx.recv().expect("waiter shares reopened value"))
            .collect();
        for value in &values[1..] {
            assert!(Arc::ptr_eq(&values[0], value));
        }
        assert_eq!(log.lock().expect("close log").len(), 1);
        drop(values);
        for _ in 0..3 {
            hold_tx.send(()).expect("release caller");
        }
        for caller in callers {
            caller.join().expect("caller thread");
        }
        assert!(lifecycle.lock().is_empty());
        assert_eq!(*log.lock().expect("close log"), ["closed", "closed"]);
    }
}
