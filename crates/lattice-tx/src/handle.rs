//! The sequencer: the kernel on its own OS thread behind a bounded queue
//! (ARCHITECTURE §8 — session tasks hand commands to the synchronous kernel
//! at the sequencing point), plus the writer pool that takes the subtree
//! U-locks around a submission.
//!
//! - **One thread owns the [`Kernel`].** Callers never touch it; they submit
//!   closures and get a receiver for the result.
//! - **Bounded queue** = backpressure at the sequencing point; the bound is a
//!   config value supplied by the caller.
//! - **Shutdown hands the kernel back** (joined, intact) so a host can
//!   checkpoint, inspect, or reopen.
//!
//! The timer path (journal time, timer-fired actions) is not part of this
//! surface yet — BACKLOG SQ-09.

use crate::command::Command;
use crate::error::{KernelError, TxError};
use crate::kernel::Kernel;
use crate::lock::SubtreeLockTable;
use crate::state::Subject;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce(&mut Kernel) + Send + 'static>;

/// One writer outcome: the caller's sequence tag and the command's result.
pub type Outcome = (u64, Result<crate::command::Reply, KernelError>);

/// A kernel on a dedicated sequencer thread.
pub struct KernelHandle {
    tx: SyncSender<Job>,
    join: Option<JoinHandle<Result<Kernel, TxError>>>,
    queue_bound: usize,
}

impl std::fmt::Debug for KernelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelHandle")
            .field("queue_bound", &self.queue_bound)
            .finish_non_exhaustive()
    }
}

impl KernelHandle {
    /// Spawn the sequencer thread over `kernel`. `queue_bound` is the
    /// sequencing-point queue depth (a config value — never a literal here).
    pub fn spawn(kernel: Kernel, queue_bound: usize) -> Result<Self, TxError> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(queue_bound.max(1));
        let join = std::thread::Builder::new()
            .name("latticedb-sequencer".into())
            .spawn(move || {
                let mut kernel = kernel;
                while let Ok(job) = rx.recv() {
                    job(&mut kernel);
                }
                Ok(kernel)
            })
            .map_err(|e| TxError::ThreadSpawn {
                reason: e.to_string(),
            })?;
        Ok(Self {
            tx,
            join: Some(join),
            queue_bound: queue_bound.max(1),
        })
    }

    /// Run one closure against the kernel, on the sequencer thread. Blocks
    /// only when the queue is full (backpressure). `Err` = the sequencer is
    /// gone (shut down or panicked).
    pub fn submit<R: Send + 'static>(
        &self,
        run: impl FnOnce(&mut Kernel) -> R + Send + 'static,
    ) -> Result<Receiver<R>, TxError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        let job: Job = Box::new(move |kernel| {
            // The receiver may have been dropped: that is the caller's
            // choice, not a kernel failure.
            let _ = tx.send(run(kernel));
        });
        self.tx.send(job).map_err(|_| TxError::Closed)?;
        Ok(rx)
    }

    /// Stop the sequencer thread and hand the kernel back intact.
    pub fn shutdown(mut self) -> Result<Kernel, TxError> {
        let join = self.join.take().ok_or(TxError::Closed)?;
        // Dropping the sender closes the queue: the thread drains what is
        // already queued, then exits and returns the kernel.
        let (tx, _) = std::sync::mpsc::sync_channel(0);
        self.tx = tx;
        join.join().map_err(|_| TxError::Closed)?
    }
}

/// Stop a shared sequencer and hand the kernel back. Fails when another
/// owner still holds the handle — the kernel cannot move while someone can
/// still submit to it.
pub fn shutdown_shared(handle: Arc<KernelHandle>) -> Result<Kernel, TxError> {
    match Arc::try_unwrap(handle) {
        Ok(unique) => unique.shutdown(),
        Err(_) => Err(TxError::Invariant {
            detail: "kernel handle is still shared — drop every clone first".into(),
        }),
    }
}

impl Drop for KernelHandle {
    fn drop(&mut self) {
        // Dropping the sender lets a leaked handle's thread exit on its own.
        let (tx, _) = std::sync::mpsc::sync_channel(0);
        self.tx = tx;
    }
}

/// The conflict domain of a command (ARCHITECTURE §8): the target node, plus
/// its parent for structure changes.
pub fn conflict_domain(cmd: &Command) -> Vec<lattice_core::ids::NodeId> {
    use Command::*;
    match cmd {
        CreateNode { parent, .. } => parent.iter().copied().collect(),
        DeleteNode { node, .. }
        | AttrSet { node, .. }
        | CellsAppend { node, .. }
        | RowAppend { node, .. }
        | Action { node, .. } => vec![*node],
        Link { src, dst, .. } => vec![*src, *dst],
        Unlink { .. } | SchemaTxn { .. } | Grant { .. } | Revoke { .. } => Vec::new(),
    }
}

/// Writers over one sequencer: each writer takes the subtree U-locks of its
/// command's conflict domain (ordered — deadlock-free), submits, waits for
/// the receipt, releases (T-310 discipline over the durable kernel).
pub struct WriterPool {
    locks: Arc<SubtreeLockTable>,
    outcomes: Arc<Mutex<Vec<Outcome>>>,
    tx: SyncSender<(u64, Command)>,
    join: Vec<JoinHandle<()>>,
}

impl WriterPool {
    /// Spawn `workers` writer threads pulling commands from a bounded queue.
    pub fn spawn(
        handle: Arc<KernelHandle>,
        workers: usize,
        queue_bound: usize,
    ) -> Result<Self, TxError> {
        let locks = Arc::new(SubtreeLockTable::new());
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Command)>(queue_bound.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let mut join = Vec::with_capacity(workers.max(1));
        for i in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            let locks = Arc::clone(&locks);
            let handle = Arc::clone(&handle);
            let outcomes = Arc::clone(&outcomes);
            let spawned = std::thread::Builder::new()
                .name(format!("latticedb-writer-{i}"))
                .spawn(move || writer_loop(rx, locks, handle, outcomes));
            match spawned {
                Ok(h) => join.push(h),
                Err(e) => {
                    drop(tx);
                    for h in join {
                        let _ = h.join();
                    }
                    return Err(TxError::ThreadSpawn {
                        reason: e.to_string(),
                    });
                }
            }
        }
        Ok(Self {
            locks,
            outcomes,
            tx,
            join,
        })
    }

    /// Enqueue a command under its sequence tag (blocks when the queue is
    /// full — backpressure at the sequencing point).
    pub fn enqueue(&self, seq: u64, cmd: Command) -> Result<(), TxError> {
        self.tx.send((seq, cmd)).map_err(|_| TxError::Closed)
    }

    pub fn locks(&self) -> &SubtreeLockTable {
        &self.locks
    }

    /// Drain: close the queue, join the writers, and hand back every outcome
    /// as `(seq, result)` in completion order. A poisoned outcome lock is a
    /// pool fault, not an empty history.
    pub fn shutdown(self) -> Result<Vec<Outcome>, TxError> {
        drop(self.tx);
        for h in self.join {
            h.join().map_err(|_| TxError::Invariant {
                detail: "a writer thread panicked".into(),
            })?;
        }
        let outcomes = Arc::try_unwrap(self.outcomes)
            .map_err(|_| TxError::Invariant {
                detail: "writer pool outcomes are still shared".into(),
            })?
            .into_inner()
            .map_err(|_| TxError::LockPoisoned)?;
        Ok(outcomes)
    }
}

fn writer_loop(
    rx: Arc<Mutex<Receiver<(u64, Command)>>>,
    locks: Arc<SubtreeLockTable>,
    handle: Arc<KernelHandle>,
    outcomes: Arc<Mutex<Vec<Outcome>>>,
) {
    loop {
        let (seq, cmd) = {
            let Ok(guard) = rx.lock() else {
                return;
            };
            match guard.recv() {
                Ok(item) => item,
                Err(_) => return, // queue closed → drain-and-exit
            }
        };
        let domain = conflict_domain(&cmd);
        let Ok(_guard) = locks.acquire(domain) else {
            return;
        };
        // Block on the receipt while holding the U-locks: per-node fact
        // order is preserved because this writer's next command for the same
        // domain cannot lock until this one releases.
        let outcome = match handle
            .submit(move |kernel| kernel.execute(0, &Subject::trusted("writer"), &cmd))
        {
            Ok(rx) => match rx.recv() {
                Ok(result) => result,
                Err(_) => return, // sequencer gone
            },
            Err(_) => return,
        };
        // A poisoned outcomes lock means the pool is already lost; record
        // nothing rather than block a writer forever.
        if let Ok(mut sink) = outcomes.lock() {
            sink.push((seq, outcome));
        }
    }
}
