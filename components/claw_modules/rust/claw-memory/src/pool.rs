//! `MemoryTaskPool` — a fixed set of background workers shared by the memory
//! subsystem.
//!
//! Why a pool instead of spawning per job: on ESP-IDF (FreeRTOS) creating and
//! tearing down a task per piece of work churns the heap and risks fragmentation
//! and stack-allocation failures. The C firmware policy is to allocate worker
//! tasks once and keep them. This pool mirrors that: it spawns
//! [`PoolConfig::workers`] threads at construction (via [`claw_sys`], which gives
//! them a PSRAM-backed stack on device) and they live until the pool is dropped.
//!
//! Every memory type (conversation compaction today; profile / long-term
//! extraction later) submits its background work here as a [`MemoryJob`]. The
//! pool is intentionally dumb: FIFO, no priorities, no result channel. A job
//! that needs to report back captures its own `Arc`/channel.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use claw_sys::thread::{self, NO_AFFINITY};

/// A unit of background memory work. Runs to completion on one worker thread.
pub type MemoryJob = Box<dyn FnOnce() + Send + 'static>;

/// Default worker stack size.
///
/// Compaction summarization can reach the LLM backend (HTTP + TLS + serde_json),
/// the deepest workload these workers run, so the stack is sized for it. Matches
/// the order of magnitude of the C agent worker stacks.
const DEFAULT_STACK_SIZE: usize = 32 * 1024;

/// Default worker priority (FreeRTOS task priority; ignored on host).
const DEFAULT_PRIORITY: u32 = 5;

/// Configuration for a [`MemoryTaskPool`].
pub struct PoolConfig {
    /// Number of persistent worker threads. One is enough for serialized
    /// compaction; raise it when several memory types run heavy jobs at once.
    pub workers: usize,
    /// Per-worker stack size in bytes.
    pub stack_size: usize,
    /// Per-worker priority (FreeRTOS; ignored on host).
    pub priority: u32,
    /// Core affinity ([`NO_AFFINITY`] to let the scheduler choose).
    pub core: i32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            stack_size: DEFAULT_STACK_SIZE,
            priority: DEFAULT_PRIORITY,
            core: NO_AFFINITY,
        }
    }
}

/// State shared between the pool handle and every worker.
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a job is enqueued or shutdown is requested.
    signal: Condvar,
}

struct Queue {
    jobs: VecDeque<MemoryJob>,
    /// Set on drop; tells idle workers to exit once the queue drains.
    shutdown: bool,
}

/// A fixed pool of background workers for memory jobs.
///
/// Create one at boot with [`MemoryTaskPool::new`] and share it (via `Arc`)
/// across every memory type; submit work with [`submit`](Self::submit). Workers
/// run until the pool is dropped.
///
/// # Examples
///
/// ```
/// use std::sync::{Arc, mpsc};
///
/// use claw_memory::{MemoryTaskPool, PoolConfig};
///
/// let pool = MemoryTaskPool::new(PoolConfig::default())?;
///
/// // Submit a job and wait for it to run on a worker thread.
/// let (tx, rx) = mpsc::channel();
/// pool.submit(Box::new(move || {
///     tx.send(2 + 2).unwrap();
/// }));
/// assert_eq!(rx.recv().unwrap(), 4);
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct MemoryTaskPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl MemoryTaskPool {
    /// Spawn the worker threads. They block on an empty queue until work arrives.
    ///
    /// Returns the OS error if a worker thread cannot be spawned; on device that
    /// means the task/stack allocation failed.
    ///
    /// # Examples
    ///
    /// ```
    /// use claw_memory::{MemoryTaskPool, PoolConfig};
    ///
    /// // One worker is enough for serialized compaction; raise `workers` for more.
    /// let pool = MemoryTaskPool::new(PoolConfig { workers: 2, ..PoolConfig::default() })?;
    /// # let _ = pool;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn new(config: PoolConfig) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            signal: Condvar::new(),
        });

        let mut workers = Vec::with_capacity(config.workers);
        for index in 0..config.workers {
            let shared = Arc::clone(&shared);
            let name = format!("claw_mem_{index}");
            let handle = thread::spawn_worker(
                &name,
                config.stack_size,
                config.priority,
                config.core,
                move || worker_loop(&shared),
            )?;
            workers.push(handle);
        }

        Ok(Self { shared, workers })
    }

    /// Enqueue a job. It runs on the next free worker, in submission order.
    ///
    /// Best-effort: if the pool is shutting down the job is dropped unrun.
    pub fn submit(&self, job: MemoryJob) {
        let mut queue = lock(&self.shared.queue);
        if queue.shutdown {
            return;
        }
        queue.jobs.push_back(job);
        drop(queue);
        self.shared.signal.notify_one();
    }
}

impl Drop for MemoryTaskPool {
    fn drop(&mut self) {
        {
            let mut queue = lock(&self.shared.queue);
            queue.shutdown = true;
        }
        self.shared.signal.notify_all();
        for handle in self.workers.drain(..) {
            // A worker only exits its loop after observing shutdown, so joining
            // cannot deadlock. Ignore the panic payload if a job unwound.
            let _ = handle.join();
        }
    }
}

/// Worker body: take jobs until shutdown drains the queue.
fn worker_loop(shared: &Shared) {
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            loop {
                if let Some(job) = queue.jobs.pop_front() {
                    break job;
                }
                if queue.shutdown {
                    return;
                }
                queue = wait(&shared.signal, queue);
            }
        };
        // Run the job with the lock released so it never blocks submitters or
        // other workers (compaction can be slow — it may hit the network).
        job();
    }
}

/// Lock a mutex, recovering the guard if a previous job panicked while holding
/// it. The pool's invariants don't depend on the poisoned data being pristine,
/// so recovering is preferable to propagating a panic into unrelated workers.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `Condvar::wait` with the same poison-recovery policy as [`lock`].
fn wait<'a, T>(
    signal: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    signal
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
