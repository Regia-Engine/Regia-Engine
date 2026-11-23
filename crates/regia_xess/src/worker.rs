// SPDX-License-Identifier: MPL-2.0
//! Synchronous owned calls to one SDK thread (Intel's XeSS thread-affinity contract).
use crate::{Error, Version};
use std::sync::mpsc;
use std::thread;

type Job<T> = Box<dyn FnOnce(&T) + Send>;
pub(crate) struct Worker<T> {
    sender: Option<mpsc::Sender<Job<T>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl<T: 'static> Worker<T> {
    pub(crate) fn spawn(
        create: impl FnOnce() -> Result<(T, Version), Error> + Send + 'static,
    ) -> Result<(Self, Version), Error> {
        let (sender, receiver) = mpsc::channel::<Job<T>>();
        let (ready, version) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("regia-xess".into())
            .spawn(move || {
                match create() {
                    Ok((sdk, version)) => {
                        if ready.send(Ok(version)).is_err() {
                            return;
                        }
                        for job in receiver {
                            job(&sdk);
                        }
                        // The SDK library is also unloaded on its initialization thread.
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        let worker = Self {
            sender: Some(sender),
            thread: Some(thread),
        };
        let version = version.recv().map_err(|e| Error::Backend(e.to_string()))??;
        Ok((worker, version))
    }
    // Returning only after the result arrives also synchronizes the narrowly scoped raw
    // feature-chain borrow in native::request_device. A dropped worker disconnects this call
    // only after its closure has unwound; it cannot continue accessing a caller's stack.
    pub(crate) fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&T) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| Error::Backend("SDK worker stopped".into()))?
            .send(Box::new(move |sdk| {
                let _ = reply.send(work(sdk));
            }))
            .map_err(|e| Error::Backend(e.to_string()))?;
        result.recv().map_err(|e| Error::Backend(e.to_string()))?
    }
}
impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::Arc};
    const VERSION: Version = Version {
        major: 2,
        minor: 0,
        patch: 2,
    };
    #[test]
    fn initialization_calls_and_destruction_share_one_thread() {
        let (drop_tx, drop_rx) = mpsc::channel();
        struct Sdk {
            owner: thread::ThreadId,
            calls: Cell<u32>,
            dropped: mpsc::Sender<(thread::ThreadId, u32)>,
        }
        impl Drop for Sdk {
            fn drop(&mut self) {
                let _ = self.dropped.send((thread::current().id(), self.calls.get()));
            }
        }
        let (worker, _) = Worker::spawn(move || {
            Ok((
                Sdk {
                    owner: thread::current().id(),
                    calls: Cell::new(0),
                    dropped: drop_tx,
                },
                VERSION,
            ))
        })
        .expect("worker");
        let worker = Arc::new(worker);
        let owner = worker.call(|sdk| Ok(sdk.owner)).expect("owner");
        assert_ne!(owner, thread::current().id());
        let callers: Vec<_> = (0..4)
            .map(|_| {
                let worker = Arc::clone(&worker);
                thread::spawn(move || {
                    for _ in 0..8 {
                        worker
                            .call(|sdk| {
                                assert_eq!(sdk.owner, thread::current().id());
                                sdk.calls.set(sdk.calls.get() + 1);
                                Ok(())
                            })
                            .expect("serialized call");
                    }
                })
            })
            .collect();
        for caller in callers {
            caller.join().expect("caller");
        }
        drop(worker);
        assert_eq!(drop_rx.recv().expect("destruction"), (owner, 32));
    }
    #[test]
    fn failed_initialization_propagates_and_joins() {
        assert!(matches!(
            Worker::<()>::spawn(|| Err(Error::UnsupportedVersion)),
            Err(Error::UnsupportedVersion)
        ));
    }
    #[test]
    fn panicking_worker_disconnects_the_waiter() {
        let (worker, _) = Worker::spawn(|| Ok(((), VERSION))).expect("worker");
        assert!(matches!(
            worker.call::<()>(|_| panic!("deliberate SDK-worker test panic")),
            Err(Error::Backend(_))
        ));
    }
}
