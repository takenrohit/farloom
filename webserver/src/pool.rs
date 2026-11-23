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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    fn current_name() -> String {
        thread::current().name().unwrap_or("<unnamed>").to_string()
    }

    #[test]
    fn pool_of_three_runs_jobs_on_three_named_workers_at_once() {
        let pool = closure_pool(3, 3).expect("spawn pool");
        assert_eq!(pool.size(), 3);

        let (started_tx, started_rx) = mpsc::channel::<String>();
        let mut releases = Vec::new();
        for _ in 0..3 {
            let (release_tx, release_rx) = mpsc::channel::<()>();
            releases.push(release_tx);
            let started_tx = started_tx.clone();
            let job: Job = Box::new(move || {
                started_tx.send(current_name()).unwrap();
                // Hold this worker until the test releases it.
                release_rx.recv_timeout(WAIT).expect("release timed out");
            });
            assert!(pool.try_submit(job).is_ok());
        }

        // All three jobs report in while each still blocks its worker, so three
        // distinct workers are busy at the same time.
        let mut names = BTreeSet::new();
        for _ in 0..3 {
            names.insert(started_rx.recv_timeout(WAIT).expect("job did not start"));
        }
        let expected: BTreeSet<String> =
            (0..3).map(|i| format!("worker-{i}")).collect();
        assert_eq!(names, expected);

        for r in &releases {
            r.send(()).unwrap();
        }
        drop(pool);
    }

    #[test]
    fn third_submission_to_busy_single_worker_with_capacity_one_is_full() {
        let (started_tx, started_rx) = mpsc::channel::<u32>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let started_tx = Mutex::new(started_tx);
        let release_rx = Mutex::new(release_rx);

        let pool = ThreadPool::new(1, 1, move |id: u32| {
            started_tx.lock().unwrap().send(id).unwrap();
            if id == 1 {
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(WAIT)
                    .expect("release timed out");
            }
        })
        .expect("spawn pool");

        assert!(pool.try_submit(1).is_ok());
        assert_eq!(started_rx.recv_timeout(WAIT).expect("job 1 did not start"), 1);

        // The worker is held by job 1, so job 2 fills the single queue slot.
        assert!(pool.try_submit(2).is_ok());
        match pool.try_submit(3) {
            Err(SubmitError::Full(j)) => assert_eq!(j, 3),
            other => panic!("expected Full(3), got {other:?}"),
        }

        release_tx.send(()).unwrap();
        assert_eq!(started_rx.recv_timeout(WAIT).expect("job 2 did not run"), 2);
        drop(pool);
        assert!(started_rx.try_recv().is_err(), "rejected job 3 must not run");
    }

    #[test]
    fn drop_runs_every_queued_job_before_returning() {
        const M: usize = 6;
        let pool = closure_pool(1, M + 1).expect("spawn pool");
        let counter = Arc::new(AtomicUsize::new(0));

        // Gate the only worker so the counter jobs sit in the queue.
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let gate: Job = Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).expect("release timed out");
        });
        assert!(pool.try_submit(gate).is_ok());
        entered_rx.recv_timeout(WAIT).expect("gate job did not start");

        for _ in 0..M {
            let c = Arc::clone(&counter);
            let job: Job = Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
            assert!(pool.try_submit(job).is_ok());
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0, "jobs must still be queued");

        release_tx.send(()).unwrap();
        drop(pool);
        assert_eq!(counter.load(Ordering::SeqCst), M);
    }

    #[test]
    fn panicking_job_does_not_kill_single_worker() {
        let pool = closure_pool(1, 4).expect("spawn pool");
        let (tx, rx) = mpsc::channel::<String>();

        let boom: Job = Box::new(|| panic!("intentional test panic"));
        assert!(pool.try_submit(boom).is_ok());

        for _ in 0..2 {
            let tx = tx.clone();
            let job: Job = Box::new(move || tx.send(current_name()).unwrap());
            assert!(pool.try_submit(job).is_ok());
        }

        for _ in 0..2 {
            let name = rx.recv_timeout(WAIT).expect("job after panic did not run");
            assert_eq!(name, "worker-0");
        }
        drop(pool);
    }
}
