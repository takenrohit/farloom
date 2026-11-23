//! Fixed-size thread pool with a bounded job queue, built on `std::sync::mpsc::sync_channel`.
//!
//! One `SyncSender` (owned by the pool) feeds a channel whose `Receiver` is shared by every
//! worker behind `Arc<Mutex<_>>`. `try_submit` never blocks: when `capacity` jobs are already
//! buffered it hands the job back as `SubmitError::Full(job)`. Dropping the pool drops the
//! sender, which closes the queue; workers keep receiving the buffered jobs and exit once
//! `recv()` reports `RecvError`. `Drop` then joins every worker.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use crate::log;

/// A pool of `size` worker threads that run a shared handler on each submitted payload.
pub struct ThreadPool<J: Send + 'static> {
    /// `None` once `Drop` starts.
    sender: Option<mpsc::SyncSender<J>>,
    workers: Vec<Worker>,
}

struct Worker {
    id: usize,
    handle: Option<JoinHandle<()>>,
}

/// Why `try_submit` refused a job. The payload is always handed back.
#[derive(Debug)]
pub enum SubmitError<J> {
    /// The queue already holds `capacity` jobs.
    Full(J),
    /// Every worker is gone, so nobody can receive the job.
    Closed(J),
}

impl<J: Send + 'static> ThreadPool<J> {
    /// Spawns `size` threads named `worker-0`..`worker-{size-1}` sharing a queue of `capacity`.
    ///
    /// Panics if `size == 0` or `capacity == 0`. If a spawn fails, the workers already
    /// started are shut down and joined before the error is returned.
    pub fn new<H>(size: usize, capacity: usize, handler: H) -> std::io::Result<Self>
    where
        H: Fn(J) + Send + Sync + 'static,
    {
        assert!(size > 0, "thread pool size must be at least 1");
        assert!(capacity > 0, "thread pool queue capacity must be at least 1");

        let (tx, rx) = mpsc::sync_channel::<J>(capacity);
        let rx = Arc::new(Mutex::new(rx));
        let handler = Arc::new(handler);

        // Build the pool first so an early `?` return drops it, which closes the channel
        // and joins the workers spawned so far.
        let mut pool = ThreadPool {
            sender: Some(tx),
            workers: Vec::with_capacity(size),
        };

        for id in 0..size {
            let rx = Arc::clone(&rx);
            let handler = Arc::clone(&handler);
            let handle = thread::Builder::new()
                .name(format!("worker-{id}"))
                .spawn(move || worker_loop(id, &rx, &*handler))?;
            pool.workers.push(Worker {
                id,
                handle: Some(handle),
            });
        }

        Ok(pool)
    }

    /// Buffers `job` without blocking, or hands it back if the queue is full or closed.
    pub fn try_submit(&self, job: J) -> Result<(), SubmitError<J>> {
        match &self.sender {
            Some(tx) => tx.try_send(job).map_err(|e| match e {
                mpsc::TrySendError::Full(j) => SubmitError::Full(j),
                mpsc::TrySendError::Disconnected(j) => SubmitError::Closed(j),
            }),
            None => Err(SubmitError::Closed(job)),
        }
    }

    /// Number of worker threads.
    pub fn size(&self) -> usize {
        self.workers.len()
    }
}

fn worker_loop<J, H>(id: usize, rx: &Mutex<mpsc::Receiver<J>>, handler: &H)
where
    H: Fn(J),
{
    loop {
        // The guard is a temporary, so the mutex is released at the end of this statement,
        // before the job runs. `while let Ok(job) = rx.lock().unwrap().recv()` would keep the
        // guard alive for the loop body and serialize every job onto one worker.
        let next = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
        match next {
            Ok(job) => {
                let result = panic::catch_unwind(AssertUnwindSafe(|| handler(job)));
                if let Err(payload) = result {
                    log::error("job_panic", &panic_message(&*payload));
                }
            }
            Err(mpsc::RecvError) => {
                log::info_event("worker_exit", &format!("worker=worker-{id}"));
                break;
            }
        }
    }
}

impl<J: Send + 'static> Drop for ThreadPool<J> {
    fn drop(&mut self) {
        // Closing the queue: buffered jobs stay receivable, then `recv` returns `RecvError`.
        drop(self.sender.take());
        for worker in &mut self.workers {
            if let Some(handle) = worker.handle.take() {
                // Err only if the thread panicked outside the per-job `catch_unwind`.
                if handle.join().is_err() {
                    log::error("worker_join", &format!("worker=worker-{} panicked", worker.id));
                }
            }
        }
    }
}

/// Extracts a readable message from a panic payload.
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// A boxed closure job, for pools where the handler just calls the job.
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// A pool whose jobs are closures: the handler is `|job| job()`.
pub fn closure_pool(size: usize, capacity: usize) -> std::io::Result<ThreadPool<Job>> {
    ThreadPool::new(size, capacity, |job: Job| job())
}
