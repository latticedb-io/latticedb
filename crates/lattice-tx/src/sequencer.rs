//! The single sequencer (ARCH §8, P15): ONE thread assigns LSNs and
//! appends the journal. Validation runs node-locally at sequencing time
//! against the engine gate — right before the append — so the journal's
//! fact order is exactly the validation order and any interleave converges
//! to the journal's own replay (confluence).
//!
//! Two drive modes:
//! - inline: `ingest()` called on the owning thread (deterministic tests).
//! - threaded: `spawn()` runs an mpsc worker; `submit()` returns a
//!   receipt channel (real writer-pool path).

use crate::{Fact, SequenceReceipt, TxError};
use lattice_core::ids::Lsn;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

/// The engine validation gate — called by the sequencer immediately
/// before the journal append (validation at sequencing time, ARCH §8).
/// Applies the fact to the engine state on success (the engine IS the
/// state the journal replays into).
pub trait SequencerGate: Send {
    fn validate_and_apply(&mut self, fact: &Fact) -> Result<(), TxError>;
}

/// Journal sink — one txn per fact. Real host: storage::journal::Journal.
/// The sequencer guarantees dense LSNs 1..n (gap-free law).
pub trait JournalSink: Send {
    fn begin(&mut self, txn_id: u64) -> Result<(), TxError>;
    fn append(&mut self, txn_id: u64, lsn: u64, payload: &[u8]) -> Result<(), TxError>;
    fn commit(&mut self, txn_id: u64, first_lsn: u64, last_lsn: u64) -> Result<(), TxError>;
    fn records_len(&self) -> usize;
}

/// In-memory journal sink (tests + staging; the durable host adapter
/// wraps storage::journal::Journal in the Host binary).
#[derive(Default)]
pub struct MemoryJournal {
    pub records: Vec<(u64, Vec<u8>)>,
    next_lsn: u64,
}

impl JournalSink for MemoryJournal {
    fn records_len(&self) -> usize {
        self.records.len()
    }
    fn begin(&mut self, _txn_id: u64) -> Result<(), TxError> {
        Ok(())
    }
    fn append(&mut self, _txn_id: u64, lsn: u64, payload: &[u8]) -> Result<(), TxError> {
        if lsn != self.next_lsn + 1 {
            return Err(TxError::Journal {
                reason: format!("lsn gap: expected {}, got {}", self.next_lsn + 1, lsn),
            });
        }
        self.next_lsn = lsn;
        self.records.push((lsn, payload.to_vec()));
        Ok(())
    }
    fn commit(&mut self, _txn_id: u64, _first: u64, _last: u64) -> Result<(), TxError> {
        Ok(())
    }
}

/// Sequencer state — owned by exactly one thread (inline mode: the test
/// or host thread; threaded mode: the spawned worker).
pub struct Sequencer<G: SequencerGate, J: JournalSink> {
    gate: G,
    journal: J,
    next_lsn: u64,
    next_txn_id: u64,
}

impl<G: SequencerGate, J: JournalSink> Sequencer<G, J> {
    pub fn new(gate: G, journal: J) -> Self {
        Self {
            gate,
            journal,
            next_lsn: 0,
            next_txn_id: 1,
        }
    }

    /// Ingest one fact: gate-validate → journal-begin/append/commit →
    /// LSN. THE write-path bottleneck by design (P15 single writer).
    pub fn ingest(&mut self, fact: &mut Fact) -> SequenceReceipt {
        self.gate.validate_and_apply(fact)?;
        let txn_id = self.next_txn_id;
        self.next_txn_id += 1;
        self.journal.begin(txn_id)?;
        let lsn = self.next_lsn + 1;
        let payload = crate::sequencer::encode_fact(fact, lsn);
        self.journal.append(txn_id, lsn, &payload)?;
        self.journal.commit(txn_id, lsn, lsn)?;
        self.next_lsn = lsn;
        Ok(Lsn(lsn))
    }

    pub fn next_lsn(&self) -> u64 {
        self.next_lsn + 1
    }

    pub fn journal_len(&self) -> usize {
        self.journal.records_len()
    }

    /// Durable journal access for replay tests.
    pub fn journal(&self) -> &J {
        &self.journal
    }

    pub fn gate(&mut self) -> &mut G {
        &mut self.gate
    }
}

/// Fact → journal payload (deterministic encoding; replay uses the same
/// encoding — the journal is the only authority, P7).
pub fn encode_fact(fact: &Fact, lsn: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&lsn.to_le_bytes());
    out.extend_from_slice(&fact.txn_id.to_le_bytes());
    match &fact.payload {
        crate::FactPayload::AttrSet {
            node,
            attr,
            k,
            value,
        } => {
            out.push(1);
            out.extend_from_slice(&node.0.to_le_bytes());
            out.extend_from_slice(&attr.0.to_le_bytes());
            out.extend_from_slice(&k.0.to_le_bytes());
            encode_value(value, &mut out);
        }
        crate::FactPayload::CellsAppend {
            node,
            attr,
            k,
            value,
        } => {
            out.push(2);
            out.extend_from_slice(&node.0.to_le_bytes());
            out.extend_from_slice(&attr.0.to_le_bytes());
            out.extend_from_slice(&k.0.to_le_bytes());
            encode_value(value, &mut out);
        }
        crate::FactPayload::CreateNode { parent, type_code } => {
            out.push(3);
            match parent {
                Some(p) => {
                    out.push(1);
                    out.extend_from_slice(&p.0.to_le_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&type_code.to_le_bytes());
        }
        crate::FactPayload::DeleteNode { node, cascade } => {
            out.push(4);
            out.extend_from_slice(&node.0.to_le_bytes());
            out.push(u8::from(*cascade));
        }
    }
    out
}

fn encode_value(value: &lattice_core::value::Value, out: &mut Vec<u8>) {
    use lattice_core::value::Value as V;
    match value {
        V::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        V::I64(v) => {
            out.push(2);
            out.extend_from_slice(&v.to_le_bytes());
        }
        V::F64(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_le_bytes());
        }
        V::Str(s) => {
            out.push(5);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        V::Timestamp(v) => {
            out.push(6);
            out.extend_from_slice(&v.to_le_bytes());
        }
        _ => {
            // Deterministic degrade for non-inline kinds (engine gate
            // rejects non-inline kinds before this point in the real path).
            out.push(0);
        }
    }
}

/// Threaded mode: one worker thread owning the sequencer.
pub struct SequencerHandle<G: SequencerGate + 'static, J: JournalSink + 'static> {
    tx: Sender<(Fact, Sender<SequenceReceipt>)>,
    join: Option<JoinHandle<()>>,
    _marker: std::marker::PhantomData<fn() -> (G, J)>,
}

impl<G: SequencerGate + 'static, J: JournalSink + 'static> SequencerHandle<G, J> {
    /// Spawn the sequencer thread (P15: exactly one).
    pub fn spawn(sequencer: Sequencer<G, J>) -> Result<Self, TxError> {
        let (tx, rx): (Sender<(Fact, Sender<SequenceReceipt>)>, Receiver<_>) =
            std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name("latticedb-sequencer".into())
            .spawn(move || sequencer_run(sequencer, rx))
            .map_err(|e| TxError::ThreadSpawn {
                reason: e.to_string(),
            })?;
        Ok(Self {
            tx,
            join: Some(join),
            _marker: std::marker::PhantomData,
        })
    }

    /// Submit one fact; the receipt arrives on the returned channel.
    pub fn submit(&self, fact: Fact) -> Receiver<SequenceReceipt> {
        let (rtx, rrx) = std::sync::mpsc::channel();
        if self.tx.send((fact, rtx.clone())).is_err() {
            let _ = rtx.send(Err(TxError::Closed));
        }
        rrx
    }

    pub fn shutdown(mut self) {
        drop(self.tx);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn sequencer_run<G: SequencerGate, J: JournalSink>(
    mut sequencer: Sequencer<G, J>,
    rx: Receiver<(Fact, Sender<SequenceReceipt>)>,
) {
    for (mut fact, reply) in rx {
        let receipt = sequencer.ingest(&mut fact);
        let _ = reply.send(receipt);
    }
}

/// Shared handle for pools.
pub type SharedSequencerHandle<G, J> = Arc<SequencerHandle<G, J>>;
