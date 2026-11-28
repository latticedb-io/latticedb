//! The kernel: one database's engine state under its durable journal.
//!
//! Owns the lifecycle — create / open / recover / checkpoint / close — the
//! write path, explicit transactions and the watch emission point. All
//! methods take `&mut self`: the kernel is single-threaded by construction;
//! sessions reach it through [`crate::handle::KernelHandle`] (one sequencer
//! thread, bounded queue — ARCHITECTURE §8).
//!
//! **Write path (ARCHITECTURE §5.1):** plan → journal (`BEGIN` + records +
//! `COMMIT` in one call) → apply each record with its own LSN → quiesce →
//! publish events → sync. A failure after the journal write poisons the
//! kernel ([`TxError::Invariant`]): memory may differ from the journal, so
//! it refuses every later command until reopened.
//!
//! **Recovery (ARCHITECTURE §5.3):** MANIFEST → snapshot (structure +
//! catalog) → parts (cells) → journal replay from the checkpoint LSN through
//! the very [`EngineState::apply`] live execution uses.
//!
//! **Explicit transactions (SPEC-02 §5):** `begin_txn` clones the engine
//! state into a session-private scratch; the session's commands are planned
//! and applied on the scratch (read-your-writes, ids visible) and their
//! records buffered — nothing is journaled until `commit_txn`. A failing
//! command aborts with an error naming it: nothing applied, no LSN consumed
//! (WPT-5.2). `commit_txn` journals and applies the buffered records on the
//! live state; if any other unit reached the live state since `begin_txn`,
//! the commit is refused `ORDER_CONFLICT` instead of guessing. One explicit
//! txn per database; other sessions keep writing to the live state.

use crate::command::{Command, Reply};
use crate::error::{KernelError, TxError, codes, invariant};
use crate::records::{Record, RecordData};
use crate::state::{EngineState, Event, Subject};
use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{AttrId, EdgeId, GraphId, NodeId};
use lattice_core::value::Value;
use lattice_schema::catalog::Catalog;
use lattice_schema::entities::{AttrType, ElemType, ScalarType};
use lattice_schema::key::Key;
use lattice_schema::opslog;
use lattice_schema::txn::{OpResult, SchemaOp, SchemaState};
use lattice_storage::codecs::{ColumnType, default_chain};
use lattice_storage::journal::{self, JournalTxn, JournalWriter, RecordKind, TxnLsns};
use lattice_storage::manifest::{self, Manifest};
use lattice_storage::part::{Part, PartBuilder, PartCell, PartImage, PartValueType};
use lattice_storage::snapshot::{self, SnapshotData};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// Kernel tunables. Every value comes from host configuration — SPEC-02 §10
/// law: no module carries a business-relevant literal. Library callers pass
/// them explicitly; the Host derives them from its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelLimits {
    /// Journal rotation threshold (`journal.max_segment_bytes`).
    pub journal_max_segment_bytes: u64,
    /// Retained watch events; the oldest are dropped first and counted.
    pub watch_buffer_events: usize,
    /// Reader-grace window for unlinking superseded snapshots and parts.
    pub reader_grace_secs: u64,
}

/// A watch event at the journal append point. `lsn` is the COMMIT LSN of the
/// unit that produced it — the event's position in journal order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    pub lsn: u64,
    pub event: Event,
}

/// What one checkpoint wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointReport {
    pub generation: u64,
    pub snapshot_id: u64,
    pub parts: usize,
    pub cells: u64,
    pub created_lsn: u64,
}

/// What one open recovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    pub generation: u64,
    pub txns_replayed: usize,
    pub last_lsn: u64,
    /// Bytes of a torn tail dropped from the active segment.
    pub torn_tail_dropped: u64,
}

struct OpenTxn {
    session: u64,
    scratch: EngineState,
    records: Vec<Record>,
    /// `write_seq` of the live state when the txn began.
    base_seq: u64,
}

/// One open database.
pub struct Kernel {
    dir: PathBuf,
    database_id: u32,
    limits: KernelLimits,
    state: EngineState,
    journal: JournalWriter,
    manifest: Manifest,
    events: VecDeque<WatchEvent>,
    events_dropped: u64,
    txn: Option<OpenTxn>,
    poisoned: Option<String>,
}

fn journal_dir(dir: &Path) -> PathBuf {
    dir.join("journal")
}

fn parts_dir(dir: &Path) -> PathBuf {
    dir.join("parts")
}

fn snapshot_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("snapshot.v{id}"))
}

fn part_path(dir: &Path, id: u64) -> PathBuf {
    parts_dir(dir).join(format!("{id:012}.lpart"))
}

fn storage(reason: impl std::fmt::Display) -> TxError {
    TxError::Storage {
        reason: reason.to_string(),
    }
}

/// Apply a unit's records in order, quiesce, and close the enabled-action
/// diff. `lsns[i]` is the LSN stamped on `records[i]` (a placeholder for
/// scratch applies — the scratch is never published).
fn apply_unit(state: &mut EngineState, records: &[Record], lsns: &[u64]) -> Result<(), TxError> {
    for (rec, lsn) in records.iter().zip(lsns) {
        state.apply(rec, *lsn)?;
    }
    state.quiesce()?;
    state.end_unit()
}

impl Kernel {
    // -- lifecycle -------------------------------------------------------------

    /// Create an empty database at `dir` (SPEC-01 §17: built-in scalars, empty
    /// catalog, zero graphs, epoch 0). `database_id` is assigned by the Host
    /// (ARCHITECTURE §4); an initial snapshot persists it from the start.
    pub fn create(dir: &Path, database_id: u32, limits: KernelLimits) -> Result<Self, TxError> {
        if dir.exists() {
            let mut entries = std::fs::read_dir(dir).map_err(storage)?;
            let occupied = entries.next().transpose().map_err(storage)?.is_some();
            if occupied {
                return Err(storage(format!(
                    "cannot create a database in non-empty directory {}",
                    dir.display()
                )));
            }
        }
        std::fs::create_dir_all(parts_dir(dir)).map_err(storage)?;
        let state = EngineState::fresh(SchemaState::new(Catalog::fresh()))?;
        let journal = JournalWriter::create(&journal_dir(dir), limits.journal_max_segment_bytes)?;
        let mut kernel = Self {
            dir: dir.to_path_buf(),
            database_id,
            limits,
            state,
            journal,
            manifest: Manifest::fresh(),
            events: VecDeque::new(),
            events_dropped: 0,
            txn: None,
            poisoned: None,
        };
        kernel.manifest.snapshot_id = 1;
        kernel.write_snapshot(1)?;
        kernel.swap_manifest()?;
        Ok(kernel)
    }

    /// Open an existing database: MANIFEST → snapshot → parts → journal replay.
    pub fn open(dir: &Path, limits: KernelLimits) -> Result<(Self, OpenReport), TxError> {
        let (manifest, _tmps_removed, _orphans_removed) =
            manifest::open_db_dir(dir, limits.reader_grace_secs)?;
        let snap = snapshot::read_snapshot(&snapshot_path(dir, manifest.snapshot_id))?;
        if snap.schema_epoch != manifest.schema_epoch {
            return Err(invariant(format!(
                "MANIFEST schema epoch {} ≠ snapshot header {}",
                manifest.schema_epoch, snap.schema_epoch
            )));
        }
        let schema = schema_from_snapshot(&snap)?;
        let arrays = GraphArrays::from_raw(&snap.arrays)
            .map_err(|e| storage(format!("snapshot structure: {e}")))?;
        arrays.assert_invariants();
        let mut state = EngineState::restore(schema, arrays)?;
        load_parts(dir, &manifest, &mut state)?;
        // The writer truncates a torn tail before anyone appends (SFT-6.4).
        let (journal, info) =
            JournalWriter::open(&journal_dir(dir), limits.journal_max_segment_bytes)?;
        let mut recovery = Recovery {
            created_lsn: manifest.created_lsn,
            guards: BTreeMap::new(),
            txns_replayed: 0,
        };
        let mut failure: Option<TxError> = None;
        journal::recover(&journal_dir(dir), |txn| {
            if failure.is_none()
                && let Err(e) = recovery.on_txn(txn, &mut state)
            {
                failure = Some(e);
            }
        })?;
        if let Some(e) = failure {
            return Err(e);
        }
        // GUARD params are outside the snapshot format (BACKLOG ED-15): the
        // pre-checkpoint journal rebuilds them for every edge still alive.
        for (edge, (sigma, tau)) in std::mem::take(&mut recovery.guards) {
            if let Some(aux) = live_edge_aux(&state, edge) {
                state.restore_guard_param(aux, sigma, tau);
            }
        }
        manifest::gc_snapshots(dir, manifest.snapshot_id, limits.reader_grace_secs)?;
        let generation = manifest.generation;
        let kernel = Self {
            dir: dir.to_path_buf(),
            database_id: snap.database_id,
            limits,
            state,
            journal,
            manifest,
            events: VecDeque::new(),
            events_dropped: 0,
            txn: None,
            poisoned: None,
        };
        Ok((
            kernel,
            OpenReport {
                generation,
                txns_replayed: recovery.txns_replayed,
                last_lsn: info.last_lsn,
                torn_tail_dropped: info.torn_tail_dropped,
            },
        ))
    }

    /// Sync the journal and release the database. An open explicit txn is
    /// rolled back (nothing of it was ever journaled).
    pub fn close(mut self) -> Result<(), TxError> {
        self.txn = None;
        Ok(self.journal.sync()?)
    }

    /// Checkpoint (SPEC-03 §7): merge-all → `snapshot.v<N+1>` → journal
    /// rotation → MANIFEST swap. One part per `(graph, attribute)` carries
    /// every live cell; the snapshot carries structure + catalog.
    pub fn checkpoint(&mut self) -> Result<CheckpointReport, TxError> {
        if let Some(reason) = &self.poisoned {
            return Err(invariant(format!("kernel is poisoned ({reason})")));
        }
        let last_lsn = self.last_lsn();
        let columns = self.collect_columns();
        let first_part = self.manifest.parts.iter().max().map_or(1, |m| m + 1);
        let mut cells = 0u64;
        let mut new_parts = Vec::with_capacity(columns.len());
        for (offset, ((graph, attr), column)) in columns.iter().enumerate() {
            let id = first_part + offset as u64;
            build_part(*graph, *attr, id, column, &self.state)?.write(&part_path(&self.dir, id))?;
            cells += column.len() as u64;
            new_parts.push(id);
        }
        let snapshot_id = self.manifest.snapshot_id + 1;
        self.write_snapshot(snapshot_id)?;
        self.journal.rotate()?;
        let generation = self.manifest.generation + 1;
        self.manifest = Manifest {
            generation,
            snapshot_id,
            parts: new_parts,
            journal_segment: self.journal.segment(),
            journal_offset: last_lsn,
            schema_epoch: self.state.schema.catalog.schema_epoch,
            created_lsn: last_lsn,
        };
        self.swap_manifest()?;
        // Superseded snapshots and parts go once no reader can hold them.
        let grace = self.limits.reader_grace_secs;
        manifest::gc_snapshots(&self.dir, snapshot_id, grace)?;
        manifest::open_db_dir(&self.dir, grace)?;
        Ok(CheckpointReport {
            generation,
            snapshot_id,
            parts: self.manifest.parts.len(),
            cells,
            created_lsn: last_lsn,
        })
    }

    fn write_snapshot(&self, id: u64) -> Result<(), TxError> {
        let payload = snapshot::encode_snapshot(
            self.database_id,
            self.state.schema.catalog.schema_epoch,
            self.last_lsn(),
            self.state.arrays(),
            &opslog::encode_ops(&self.state.schema.ops_log.concat()),
            &opslog::encode_deprecations(&self.state.schema.deprecations),
        );
        snapshot::write_snapshot(&snapshot_path(&self.dir, id), &payload)?;
        Ok(())
    }

    fn swap_manifest(&self) -> Result<(), TxError> {
        manifest::swap_manifest(&self.dir, &self.manifest, false)?;
        Ok(())
    }

    /// Live cells grouped by `(graph, attr id)` in `(node, k)` order.
    fn collect_columns(&self) -> BTreeMap<(u16, u16), Vec<PartCell>> {
        let mut columns: BTreeMap<(u16, u16), Vec<PartCell>> = BTreeMap::new();
        let machine = self.state.machine();
        for ((node, attr, k), cell) in machine.cells() {
            if !machine.arrays.is_alive(node) {
                continue;
            }
            columns
                .entry((machine.arrays.graph_of(node).0, attr.0))
                .or_default()
                .push(PartCell {
                    node,
                    k,
                    lsn: cell.lsn,
                    value: Some(cell.value.clone()),
                });
        }
        columns
    }

    // -- state access ----------------------------------------------------------

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    /// The state a session reads: its own scratch while it holds the
    /// explicit txn (read-your-writes), the live state otherwise.
    pub fn read_state(&self, session: u64) -> &EngineState {
        match &self.txn {
            Some(open) if open.session == session => &open.scratch,
            _ => &self.state,
        }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn database_id(&self) -> u32 {
        self.database_id
    }

    /// LSN of the last journaled frame (0 = empty journal).
    pub fn last_lsn(&self) -> u64 {
        self.journal.next_lsn() - 1
    }

    pub fn in_txn(&self, session: u64) -> bool {
        self.txn
            .as_ref()
            .is_some_and(|open| open.session == session)
    }

    /// Dry-run a schema txn (SPEC-01 §16): per-op verdicts, nothing committed.
    pub fn schema_plan(&self, ops: &[SchemaOp]) -> Vec<OpResult> {
        self.state.schema.plan(ops)
    }

    // -- write path ------------------------------------------------------------

    /// Execute one command. Auto-commit, unless `session` holds the explicit
    /// txn — then the command joins it (SPEC-02 §5).
    pub fn execute(
        &mut self,
        session: u64,
        subject: &Subject,
        cmd: &Command,
    ) -> Result<Reply, KernelError> {
        self.check_healthy()?;
        let in_txn = self.in_txn(session);
        if in_txn && matches!(cmd, Command::SchemaTxn { .. }) {
            return Err(KernelError::new(
                codes::INVALID_KEY,
                "",
                "SCHEMA_TXN is a read/meta frame and never joins an explicit txn",
            ));
        }
        if !in_txn {
            let planned = self.state.plan(cmd, subject)?;
            self.commit_unit(planned.records)
                .map_err(|e| KernelError::new(codes::INTERNAL, "", e.to_string()))?;
            return Ok(planned.reply);
        }
        // The scratch is this session's private view until commit.
        let planned = {
            let Some(open) = self.txn.as_mut() else {
                return Err(KernelError::new(
                    codes::INTERNAL,
                    "",
                    "explicit txn vanished",
                ));
            };
            open.scratch.plan(cmd, subject)
        };
        let planned = match planned {
            Ok(planned) => planned,
            Err(err) => {
                // A failing command aborts the txn (SPEC-02 §5, WPT-5.2):
                // nothing applied, nothing journaled, the scratch is dropped.
                self.txn = None;
                return Err(err);
            }
        };
        let (applied, reply) = {
            let Some(open) = self.txn.as_mut() else {
                return Err(KernelError::new(
                    codes::INTERNAL,
                    "",
                    "explicit txn vanished",
                ));
            };
            let zeros = vec![0u64; planned.records.len()];
            let applied = apply_unit(&mut open.scratch, &planned.records, &zeros);
            open.records.extend(planned.records);
            open.scratch.write_seq += 1;
            let _ = open.scratch.take_events();
            (applied, planned.reply)
        };
        self.propagate_fatal(applied)?;
        Ok(reply)
    }

    pub fn begin_txn(&mut self, session: u64) -> Result<(), KernelError> {
        self.check_healthy()?;
        match &self.txn {
            Some(open) if open.session == session => {
                return Err(KernelError::new(codes::TXN_NESTED, "", "txn already open"));
            }
            Some(_) => {
                return Err(KernelError::new(
                    codes::LIMIT_EXCEEDED,
                    "",
                    "another session holds the explicit txn of this database",
                ));
            }
            None => {}
        }
        let scratch = self.state.clone();
        let base_seq = self.state.write_seq;
        self.txn = Some(OpenTxn {
            session,
            scratch,
            records: Vec::new(),
            base_seq,
        });
        Ok(())
    }

    /// Commit the explicit txn: one journal txn, one LSN range (SPEC-02 §5).
    pub fn commit_txn(&mut self, session: u64) -> Result<TxnLsns, KernelError> {
        self.check_healthy()?;
        let open = self.take_txn(session)?;
        if self.state.write_seq != open.base_seq {
            return Err(KernelError::new(
                codes::ORDER_CONFLICT,
                "retry the txn — another writer changed the database since BEGIN",
                "the live state moved under the explicit txn; nothing applied",
            ));
        }
        self.commit_unit(open.records)
            .map_err(|e| KernelError::new(codes::INTERNAL, "", e.to_string()))
    }

    /// Roll back: drop the scratch. Nothing was journaled, nothing applied.
    pub fn rollback_txn(&mut self, session: u64) -> Result<(), KernelError> {
        self.check_healthy()?;
        self.take_txn(session)?;
        Ok(())
    }

    fn take_txn(&mut self, session: u64) -> Result<OpenTxn, KernelError> {
        match self.txn.take() {
            Some(open) if open.session == session => Ok(open),
            Some(other) => {
                let err = KernelError::new(
                    codes::LIMIT_EXCEEDED,
                    "",
                    "another session holds the explicit txn of this database",
                );
                self.txn = Some(other);
                Err(err)
            }
            None => Err(KernelError::new(codes::INVALID_KEY, "", "no open txn")),
        }
    }

    /// Journal → apply → quiesce → publish → sync. The sequencing point.
    fn commit_unit(&mut self, records: Vec<Record>) -> Result<TxnLsns, TxError> {
        let encoded: Vec<(RecordKind, Vec<u8>)> =
            records.iter().map(|r| (r.kind(), r.encode())).collect();
        let frames: Vec<(RecordKind, &[u8])> = encoded
            .iter()
            .map(|(kind, payload)| (*kind, payload.as_slice()))
            .collect();
        if let Err(e) = self.state.begin_unit(&records) {
            return Err(self.poison(e));
        }
        // The txn id is the LSN of its BEGIN frame: unique, monotone,
        // deterministic — no separate id space to persist.
        let txn_id = self.journal.next_lsn();
        let lsns = self.journal.write_txn(txn_id, &frames)?;
        let applied = apply_unit(&mut self.state, &records, &lsns.records);
        if let Err(e) = applied {
            return Err(self.poison(e));
        }
        if let Err(e) = self.publish(lsns.last) {
            return Err(self.poison(e));
        }
        self.state.write_seq += 1;
        Ok(lsns)
    }

    fn publish(&mut self, lsn: u64) -> Result<(), TxError> {
        for event in self.state.take_events() {
            if self.events.len() >= self.limits.watch_buffer_events {
                self.events.pop_front();
                self.events_dropped += 1;
            }
            self.events.push_back(WatchEvent { lsn, event });
        }
        Ok(self.journal.sync()?)
    }

    fn check_healthy(&self) -> Result<(), KernelError> {
        match &self.poisoned {
            None => Ok(()),
            Some(reason) => Err(KernelError::new(
                codes::INTERNAL,
                "the kernel stopped to protect the journal — reopen the database",
                reason.clone(),
            )),
        }
    }

    /// A post-sequencing failure: record it and hand it back.
    fn poison(&mut self, e: TxError) -> TxError {
        if self.poisoned.is_none() {
            self.poisoned = Some(e.to_string());
        }
        e
    }

    fn propagate_fatal(&mut self, result: Result<(), TxError>) -> Result<(), KernelError> {
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                let e = self.poison(e);
                Err(KernelError::new(codes::INTERNAL, "", e.to_string()))
            }
        }
    }

    // -- watches ---------------------------------------------------------------

    /// Events strictly after `after_lsn` (the caller pages by advancing its
    /// cursor to the last event's LSN), at most `max`, journal-ordered. Only
    /// the retained buffer is served.
    pub fn watch_events_since(&self, after_lsn: u64, max: usize) -> Vec<WatchEvent> {
        self.events
            .iter()
            .filter(|e| e.lsn > after_lsn)
            .take(max)
            .cloned()
            .collect()
    }

    /// Events dropped from the buffer since open.
    pub fn watch_events_dropped(&self) -> u64 {
        self.events_dropped
    }

    /// Is `node` inside the composition subtree rooted at `root`?
    pub fn in_subtree(&self, node: NodeId, root: NodeId) -> bool {
        let arrays = self.state.arrays();
        let mut cur = node;
        while let Some(parent) = arrays.parent_of(cur) {
            if cur == root {
                return true;
            }
            cur = parent;
        }
        cur == root
    }
}

// -- recovery -------------------------------------------------------------------

struct Recovery {
    created_lsn: u64,
    /// GUARD params by edge id from `ADD_EDGE` records, minus `DEL_EDGE`ed ones.
    guards: BTreeMap<u64, (Key, Key)>,
    txns_replayed: usize,
}

impl Recovery {
    /// One committed journal txn. Before the checkpoint the snapshot already
    /// holds the effects — only GUARD params need remembering; from the
    /// checkpoint on, every record replays through the live apply.
    fn on_txn(&mut self, txn: &JournalTxn, state: &mut EngineState) -> Result<(), TxError> {
        let decoded = decode_txn(txn)?;
        for (rec, _) in &decoded {
            match &rec.data {
                RecordData::AddEdge {
                    edge,
                    guard: Some((sigma, tau)),
                    ..
                } => {
                    self.guards.insert(*edge, (sigma.clone(), tau.clone()));
                }
                RecordData::DelEdge { edge } => {
                    self.guards.remove(edge);
                }
                _ => {}
            }
        }
        if txn.last_lsn <= self.created_lsn {
            return Ok(());
        }
        for (rec, lsn) in decoded {
            state.apply(&rec, lsn)?;
        }
        state.quiesce()?;
        state.end_unit()?;
        // Recovery recomputes what live execution already published.
        let _ = state.take_events();
        state.write_seq += 1;
        self.txns_replayed += 1;
        Ok(())
    }
}

fn decode_txn(txn: &JournalTxn) -> Result<Vec<(Record, u64)>, TxError> {
    let mut out = Vec::with_capacity(txn.records.len());
    for rec in &txn.records {
        if rec.kind == RecordKind::Commit {
            continue;
        }
        let decoded = Record::decode(rec.kind, &rec.payload).map_err(|e| {
            invariant(format!(
                "journal record lsn {} ({:?}) does not decode: {e}",
                rec.lsn, rec.kind
            ))
        })?;
        out.push((decoded, rec.lsn));
    }
    Ok(out)
}

/// `aux` (GUARD param id) of a live edge, if the edge exists and is alive.
fn live_edge_aux(state: &EngineState, edge: u64) -> Option<u32> {
    let index = u32::try_from(edge).ok()?;
    state
        .arrays()
        .edge(EdgeId(index))
        .filter(|r| r.alive)
        .map(|r| r.aux)
}

/// Catalog + deprecation registries from the snapshot blob (op-log replay).
fn schema_from_snapshot(snap: &SnapshotData) -> Result<SchemaState, TxError> {
    let ops = opslog::decode_ops(&snap.catalog_ops)?;
    let mut state = opslog::replay_ops(&ops)?.0;
    state.catalog.schema_epoch = snap.schema_epoch;
    state.deprecations = opslog::decode_deprecations(&snap.catalog_deprecations)?;
    Ok(state)
}

/// Parts → cells. Parts store Timestamp / Duration / NodeRef columns as
/// `I64`: the attribute definition restores the logical type.
fn load_parts(dir: &Path, manifest: &Manifest, state: &mut EngineState) -> Result<(), TxError> {
    for id in &manifest.parts {
        let bytes =
            std::fs::read(part_path(dir, *id)).map_err(|e| storage(format!("part {id}: {e}")))?;
        let part = Part::from_bytes(&bytes)?;
        let attr_key = attr_key_of(state, part.attr).ok_or_else(|| {
            invariant(format!(
                "part {id} names attr id {}, not in this epoch",
                part.attr.0
            ))
        })?;
        let def = state
            .catalog()
            .attributes
            .get(&attr_key)
            .ok_or_else(|| invariant(format!("attribute `{attr_key}` is not declared")))?;
        // Record cells decode whole (a Bytes column of core-codec bytes);
        // every other column is re-typed from its element type.
        let elem = if matches!(def.attr_type, AttrType::Record(_)) {
            None
        } else {
            Some(column_elem(&def.attr_type).map_err(|_| {
                invariant(format!(
                    "part {id}: attribute `{attr_key}` has no column type"
                ))
            })?)
        };
        for cell in part.all_cells()? {
            let Some(value) = cell.value else {
                continue;
            };
            let value = match elem.as_ref() {
                Some(elem) => {
                    retype(elem, value).map_err(|e| invariant(format!("part {id}: {e}")))?
                }
                None => value,
            };
            state.load_cell(cell.node, part.attr, cell.k.0, value, cell.lsn.0)?;
        }
    }
    Ok(())
}

fn attr_key_of(state: &EngineState, attr: AttrId) -> Option<Key> {
    state
        .machine()
        .compiled_ref()
        .attr_ids
        .iter()
        .find(|(_, id)| **id == attr)
        .map(|(k, _)| k.clone())
}

/// Restore the logical type of an `I64`-column cell.
fn retype(elem: &ElemType, value: Value) -> Result<Value, String> {
    match (elem, value) {
        (ElemType::Scalar(ScalarType::Timestamp), Value::I64(x)) => Ok(Value::Timestamp(x)),
        (ElemType::Scalar(ScalarType::Duration), Value::I64(x)) => Ok(Value::Duration(x)),
        (ElemType::Scalar(ScalarType::NodeRef), Value::I64(x)) => match u32::try_from(x) {
            Ok(id) => Ok(Value::NodeRef(NodeId(id))),
            Err(_) => Err(format!("node_ref value {x} does not fit a node id")),
        },
        (_, other) => Ok(other),
    }
}

/// The column's element type (vector element, or the scalar/enum itself).
/// Records have no column element: they ride a Bytes column of core-codec
/// bytes.
fn column_elem(ty: &AttrType) -> Result<ElemType, ()> {
    match ty {
        AttrType::Vector { elem } => Ok(elem.clone()),
        AttrType::Scalar(t) => Ok(ElemType::Scalar(*t)),
        AttrType::Enum(e) => Ok(ElemType::Enum(e.clone())),
        AttrType::Record(_) => Err(()),
    }
}

fn column_type(ty: &AttrType) -> Result<ColumnType, ()> {
    Ok(match column_elem(ty)? {
        ElemType::Enum(_) => ColumnType::Enum,
        ElemType::Scalar(t) => match t {
            ScalarType::Bool => ColumnType::Bool,
            ScalarType::I64
            | ScalarType::Timestamp
            | ScalarType::Duration
            | ScalarType::NodeRef => ColumnType::I64,
            ScalarType::F64 => ColumnType::F64,
            ScalarType::String => ColumnType::Str,
            ScalarType::Bytes => ColumnType::Bytes,
            ScalarType::Decimal { .. } => ColumnType::Dec,
        },
    })
}

/// `(precision, scale)` of a decimal attribute, scalar or vector element.
fn decimal_params_of(ty: &AttrType) -> Option<(u8, u8)> {
    match column_elem(ty).ok()? {
        ElemType::Scalar(ScalarType::Decimal { precision, scale }) => Some((precision, scale)),
        _ => None,
    }
}

/// Holes in a node's key axis (row `k_skip`) force the explicit-k form:
/// dense encoding would renumber them.
fn sparse_of(cells: &[PartCell]) -> bool {
    cells
        .windows(2)
        .any(|w| w[0].node == w[1].node && w[1].k.0 != w[0].k.0 + 1)
}

/// One part per `(graph, attribute)`: every live cell of that column.
fn build_part(
    graph: u16,
    attr: u16,
    generation: u64,
    cells: &[PartCell],
    state: &EngineState,
) -> Result<PartImage, TxError> {
    let attr_key = attr_key_of(state, AttrId(attr))
        .ok_or_else(|| invariant(format!("attr id {attr} is not in this epoch")))?;
    let def = state
        .catalog()
        .attributes
        .get(&attr_key)
        .ok_or_else(|| invariant(format!("attribute `{attr_key}` is not declared")))?;
    let mode = match &def.attr_type {
        AttrType::Record(_) => PartValueType::Record,
        ty => PartValueType::Typed(
            column_type(ty)
                .map_err(|_| invariant(format!("attribute `{attr_key}` has no column type")))?,
        ),
    };
    // Record cells ride a Bytes column of core-codec bytes.
    let chain_type = match &def.attr_type {
        AttrType::Record(_) => ColumnType::Bytes,
        ty => column_type(ty)
            .map_err(|_| invariant(format!("attribute `{attr_key}` has no column type")))?,
    };
    let chain = default_chain(chain_type);
    let mut builder = PartBuilder::new(
        GraphId(graph),
        AttrId(attr),
        generation,
        mode,
        sparse_of(cells),
        chain,
    );
    if let Some((precision, scale)) = decimal_params_of(&def.attr_type) {
        builder.decimal_params(precision, scale);
    }
    for cell in cells {
        builder.add(cell.clone());
    }
    Ok(builder.finish()?)
}
