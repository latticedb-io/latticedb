//! Writer pool (ARCH §8): N dedicated worker threads pulling commands
//! from a bounded queue. Each writer: acquire subtree U-locks (ordered) →
//! submit to the single sequencer → await receipt → release. Writers are
//! pure plumbing — all validation and ordering authority lives in the
//! sequencer (P15).

use crate::lock::SubtreeLockTable;
use crate::sequencer::{JournalSink, Sequencer, SequencerGate, SequencerHandle};
use crate::{Fact, SequenceReceipt, TxError};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Pool stats (tests + metrics).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PoolStats {
    pub submitted: u64,
    pub accepted: u64,
    pub rejected: u64,
}

/// A pool of `workers` threads over one lock table + one sequencer.
pub struct WriterPool<G: SequencerGate + 'static, J: JournalSink + 'static> {
    locks: Arc<SubtreeLockTable>,
    sequencer: Arc<SequencerHandle<G, J>>,
    tx: std::sync::mpsc::SyncSender<Fact>,
    join: Vec<JoinHandle<()>>,
}

impl<G: SequencerGate + 'static, J: JournalSink + 'static> WriterPool<G, J> {
    /// Spawn the pool. The queue is bounded (backpressure at the
    /// sequencing point, execution-model DEC). `outcomes` collects
    /// (txn_id, receipt) per ingested fact — the pool's observation sink.
    pub fn spawn(
        workers: usize,
        sequencer: Sequencer<G, J>,
        queue_bound: usize,
        outcomes: Arc<Mutex<Vec<(u64, SequenceReceipt)>>>,
    ) -> Result<Self, TxError> {
        let locks = Arc::new(SubtreeLockTable::new());
        let sequencer = Arc::new(SequencerHandle::spawn(sequencer)?);
        let (tx, rx) = std::sync::mpsc::sync_channel(queue_bound.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let mut join = Vec::with_capacity(workers);
        for i in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            let locks = Arc::clone(&locks);
            let sequencer = Arc::clone(&sequencer);
            let outcomes = Arc::clone(&outcomes);
            let handle = std::thread::Builder::new()
                .name(format!("latticedb-writer-{i}"))
                .spawn(move || writer_loop(rx, locks, sequencer, outcomes))
                .map_err(|e| TxError::ThreadSpawn {
                    reason: e.to_string(),
                })?;
            join.push(handle);
        }
        Ok(Self {
            locks,
            sequencer,
            tx,
            join,
        })
    }

    /// Enqueue one fact (blocks when the bounded queue is full).
    pub fn enqueue(&self, fact: Fact) -> Result<(), TxError> {
        self.tx.send(fact).map_err(|_| TxError::Closed)
    }

    pub fn locks(&self) -> &SubtreeLockTable {
        &self.locks
    }

    /// Drain: close the queue, join writers, stop the sequencer.
    pub fn shutdown(self) {
        drop(self.tx);
        for j in self.join {
            let _ = j.join();
        }
        if let Ok(h) = Arc::try_unwrap(self.sequencer) {
            h.shutdown();
        }
    }
}

fn writer_loop<G: SequencerGate + 'static, J: JournalSink + 'static>(
    rx: Arc<Mutex<Receiver<Fact>>>,
    locks: Arc<SubtreeLockTable>,
    sequencer: Arc<SequencerHandle<G, J>>,
    outcomes: Arc<Mutex<Vec<(u64, SequenceReceipt)>>>,
) {
    loop {
        let fact = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match guard.recv() {
                Ok(f) => f,
                Err(_) => return, // queue closed → drain-and-exit
            }
        };
        let fact_txn_id = fact.txn_id;
        // Ordered U-lock acquisition over the conflict domain.
        let domain = fact.domain.clone();
        match locks.acquire(domain) {
            Ok(_guard) => {
                let receipt: Receiver<SequenceReceipt> = sequencer.submit(fact);
                // Block on the receipt while holding the U-locks: per-node
                // fact order is preserved because the sequencer applies
                // facts in LSN order and this writer's next fact for the
                // same domain cannot lock until this one releases.
                match receipt.recv() {
                    Ok(outcome) => {
                        if let Ok(mut sink) = outcomes.lock() {
                            sink.push((fact_txn_id, outcome));
                        }
                    }
                    Err(_) => return, // sequencer gone
                }
                drop(_guard);
            }
            Err(TxError::LockPoisoned) => return,
            Err(_) => return,
        }
    }
}

/// Convenience: run facts inline through a sequencer (deterministic mode).
pub fn ingest_inline<G: SequencerGate, J: JournalSink>(
    sequencer: &mut Sequencer<G, J>,
    mut fact: Fact,
) -> SequenceReceipt {
    sequencer.ingest(&mut fact)
}
