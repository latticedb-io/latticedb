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
use crate::read::{
    Batch, Budget, CellRow, ColumnReader, ColumnSchema, CursorFilter, HistoryEvent, Read,
    ReadReply, Selector, blockers_of, column_schema, graph_view, history_events, node_view,
    select_nodes, value_bytes,
};
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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    /// NODE_READ head / tail cells per vector (SPEC-04 §4: 16 by default).
    pub node_read_preview_cells: usize,
}

/// A microsecond clock the Host injects — for EPHEMERAL session state only
/// (cursor idle expiry). P8: wall-clock never enters a predicate or a stored
/// byte; journal time is a different thing (BACKLOG SQ-09).
pub type Clock = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

/// Everything the kernel takes from its host — all of it configuration.
#[derive(Clone)]
pub struct KernelOptions {
    pub limits: KernelLimits,
    /// A cursor idle longer than this is gone; its next fetch is `NOT_FOUND`
    /// with an "expired" hint (SPEC-02 §8: default 60 s).
    pub cursor_idle_ttl_us: u64,
    pub clock: Clock,
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

/// One open cursor: a column, the node set it covers, its filters, and where
/// the last fetched batch stopped.
struct Cursor {
    session: u64,
    attr: AttrId,
    schema: ColumnSchema,
    /// `None` = every live node.
    nodes: Option<BTreeSet<u32>>,
    filter: CursorFilter,
    /// Graphs holding this column, ascending — walked in order.
    graphs: Vec<u16>,
    graph_pos: usize,
    /// The column's declared element type: part cells of I64 storage are
    /// re-typed to it (`node_ref`, `timestamp`, `duration`).
    elem: Option<ElemType>,
    /// The node being paged: its cells, decoded once, and the next position.
    buffer: Option<(u32, crate::read::RunCells, usize)>,
    /// The last node fully drained in the current graph — the reader starts
    /// after it.
    done_through: Option<u32>,
    last_touch_us: u64,
}

/// One open database.
pub struct Kernel {
    dir: PathBuf,
    database_id: u32,
    options: KernelOptions,
    state: EngineState,
    journal: JournalWriter,
    manifest: Manifest,
    events: VecDeque<WatchEvent>,
    events_dropped: u64,
    txn: Option<OpenTxn>,
    poisoned: Option<String>,
    /// Parsed parts by id — loaded on first read, dropped at checkpoint.
    parts: BTreeMap<u64, Part>,
    /// `(graph, attr id)` → the part that holds the column.
    columns: BTreeMap<(u16, u16), u64>,
    cursors: BTreeMap<u64, Cursor>,
    next_cursor_id: u64,
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

/// What [`load`] rebuilt from a database directory.
struct Loaded {
    state: EngineState,
    manifest: Manifest,
    database_id: u32,
    /// Parsed parts of the MANIFEST, to seed the read cache.
    parts: Vec<(u64, Part)>,
    /// The COMMIT LSN of the last txn replay applied (0 = none).
    epoch: u64,
    txns_replayed: usize,
    torn_tail_dropped: u64,
}

/// MANIFEST → snapshot → parts → journal replay (ARCHITECTURE §5.3). With
/// `upto_lsn` the replay stops at that LSN — a time-travel view. Read-only:
/// no writer is opened and nothing on disk is changed.
fn load(dir: &Path, upto_lsn: Option<u64>) -> Result<Loaded, TxError> {
    let manifest = manifest::read_manifest(dir)?;
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
    let parts = load_parts(dir, &manifest, &mut state)?;
    let mut recovery = Recovery {
        created_lsn: manifest.created_lsn,
        guards: BTreeMap::new(),
        txns_replayed: 0,
        epoch: 0,
    };
    let mut failure: Option<TxError> = None;
    let info = journal::recover(&journal_dir(dir), |txn| {
        if failure.is_none()
            && let Err(e) = recovery.on_txn(txn, &mut state, upto_lsn)
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
    Ok(Loaded {
        state,
        manifest,
        database_id: snap.database_id,
        parts,
        epoch: recovery.epoch,
        txns_replayed: recovery.txns_replayed,
        torn_tail_dropped: info.torn_tail_dropped,
    })
}

impl Kernel {
    // -- lifecycle -------------------------------------------------------------

    /// Create an empty database at `dir` (SPEC-01 §17: built-in scalars, empty
    /// catalog, zero graphs, epoch 0). `database_id` is assigned by the Host
    /// (ARCHITECTURE §4); an initial snapshot persists it from the start.
    pub fn create(dir: &Path, database_id: u32, options: KernelOptions) -> Result<Self, TxError> {
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
        let journal =
            JournalWriter::create(&journal_dir(dir), options.limits.journal_max_segment_bytes)?;
        let mut kernel = Self {
            dir: dir.to_path_buf(),
            database_id,
            options,
            state,
            journal,
            manifest: Manifest::fresh(),
            events: VecDeque::new(),
            events_dropped: 0,
            txn: None,
            poisoned: None,
            parts: BTreeMap::new(),
            columns: BTreeMap::new(),
            cursors: BTreeMap::new(),
            next_cursor_id: 1,
        };
        kernel.manifest.snapshot_id = 1;
        kernel.write_snapshot(1)?;
        kernel.swap_manifest()?;
        Ok(kernel)
    }

    /// Open an existing database: MANIFEST → snapshot → parts → journal replay.
    pub fn open(dir: &Path, options: KernelOptions) -> Result<(Self, OpenReport), TxError> {
        let loaded = load(dir, None)?;
        // The writer truncates a torn tail before anyone appends (SFT-6.4);
        // superseded files go here, once, by the one writer.
        let (journal, _info) =
            JournalWriter::open(&journal_dir(dir), options.limits.journal_max_segment_bytes)?;
        manifest::gc_snapshots(
            dir,
            loaded.manifest.snapshot_id,
            options.limits.reader_grace_secs,
        )?;
        manifest::open_db_dir(dir, options.limits.reader_grace_secs)?;
        let report = OpenReport {
            generation: loaded.manifest.generation,
            txns_replayed: loaded.txns_replayed,
            last_lsn: journal.next_lsn() - 1,
            torn_tail_dropped: loaded.torn_tail_dropped,
        };
        let mut kernel = Self {
            dir: dir.to_path_buf(),
            database_id: loaded.database_id,
            options,
            state: loaded.state,
            journal,
            manifest: loaded.manifest,
            events: VecDeque::new(),
            events_dropped: 0,
            txn: None,
            poisoned: None,
            parts: BTreeMap::new(),
            columns: BTreeMap::new(),
            cursors: BTreeMap::new(),
            next_cursor_id: 1,
        };
        kernel.seed_columns(loaded.parts)?;
        Ok((kernel, report))
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
        let mut column_index: BTreeMap<(u16, u16), u64> = BTreeMap::new();
        for (offset, ((graph, attr), column)) in columns.iter().enumerate() {
            let id = first_part + offset as u64;
            build_part(*graph, *attr, id, column, &self.state)?.write(&part_path(&self.dir, id))?;
            cells += column.len() as u64;
            new_parts.push(id);
            column_index.insert((*graph, *attr), id);
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
        // Every staged cell now lives in a part; the old parts are superseded.
        self.state.clear_staging();
        self.parts.clear();
        self.columns = column_index;
        // Superseded snapshots and parts go once no reader can hold them.
        let grace = self.options.limits.reader_grace_secs;
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
            if self.events.len() >= self.options.limits.watch_buffer_events {
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

    /// A storage fault during a read: the database may be damaged — record
    /// it (the kernel stops serving) and surface it.
    fn fatal(&mut self, e: TxError) -> KernelError {
        let e = self.poison(e);
        KernelError::new(codes::INTERNAL, "", e.to_string())
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

    // -- read path -------------------------------------------------------------

    /// Column index from the MANIFEST's parts (one part per column).
    fn seed_columns(&mut self, parts: Vec<(u64, Part)>) -> Result<(), TxError> {
        for (id, part) in parts {
            let column = (part.graph.0, part.attr.0);
            if self.columns.contains_key(&column) {
                return Err(invariant(format!(
                    "column ({}, {}) has two live parts",
                    column.0, column.1
                )));
            }
            self.columns.insert(column, id);
            self.parts.insert(id, part);
        }
        Ok(())
    }

    /// Make sure the column's part is parsed and cached.
    fn ensure_column(&mut self, graph: u16, attr: AttrId) -> Result<(), TxError> {
        let Some(id) = self.columns.get(&(graph, attr.0)).copied() else {
            return Ok(());
        };
        if !self.parts.contains_key(&id) {
            let bytes = std::fs::read(part_path(&self.dir, id))
                .map_err(|e| storage(format!("part {id}: {e}")))?;
            let part = Part::from_bytes(&bytes)?;
            self.parts.insert(id, part);
        }
        Ok(())
    }

    /// The engine state as of `epoch` (a COMMIT LSN), rebuilt read-only from
    /// the retained snapshot + parts + journal prefix. WPT-8.2: the live
    /// state is untouched.
    fn state_at(&mut self, epoch: u64) -> Result<(EngineState, u64), KernelError> {
        let current = self.last_lsn();
        if epoch > current {
            return Err(KernelError::new(
                codes::INVALID_KEY,
                format!("at_epoch must be ≤ {current}"),
                format!("epoch {epoch} is beyond the current epoch"),
            ));
        }
        let oldest = self.manifest.created_lsn;
        if epoch < oldest {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                format!("oldest readable epoch is {oldest} (the retained snapshot)"),
                format!("epoch {epoch} predates the retained snapshot"),
            ));
        }
        match load(&self.dir, Some(epoch)) {
            // The marker is the last COMMIT at or below `epoch` — the state
            // as of the checkpoint when no txn lies in between.
            Ok(loaded) => Ok((loaded.state, loaded.epoch.max(oldest))),
            Err(e) => Err(self.fatal(e)),
        }
    }

    /// Execute one read. It sees exactly one committed epoch.
    pub fn read(&mut self, session: u64, read: &Read) -> Result<ReadReply, KernelError> {
        self.check_healthy()?;
        match read {
            Read::Snapshot {
                graph,
                subtree,
                at_epoch,
            } => {
                let view = match at_epoch {
                    Some(epoch) => {
                        let (state, marker) = self.state_at(*epoch)?;
                        graph_view(&state, marker, graph.as_ref(), *subtree)?
                    }
                    None => graph_view(
                        self.read_state(session),
                        self.last_lsn(),
                        graph.as_ref(),
                        *subtree,
                    )?,
                };
                Ok(ReadReply::Snapshot(view))
            }
            Read::NodeRead { node } => Ok(ReadReply::Node(node_view(
                self.read_state(session),
                *node,
                self.options.limits.node_read_preview_cells,
            )?)),
            Read::CursorOpen {
                attr,
                selector,
                filter,
            } => self
                .cursor_open(session, attr, selector, filter)
                .map(ReadReply::Cursor),
            Read::CursorFetch { cursor, budget } => self
                .cursor_fetch(session, *cursor, *budget)
                .map(ReadReply::Batch),
            Read::CursorClose { cursor } => self
                .cursor_close(session, *cursor)
                .map(|()| ReadReply::Closed),
            Read::Blockers { node } => Ok(ReadReply::Blockers(blockers_of(
                self.read_state(session),
                *node,
            )?)),
            Read::History { node, range } => match self.history(*node, *range) {
                Ok(events) => Ok(ReadReply::History(events)),
                Err(e) => Err(self.fatal(e)),
            },
        }
    }

    fn cursor_open(
        &mut self,
        session: u64,
        attr: &Key,
        selector: &Selector,
        filter: &CursorFilter,
    ) -> Result<u64, KernelError> {
        let (attr_id, schema) = column_schema(&self.state, attr)?;
        let elem = self
            .state
            .catalog()
            .attributes
            .get(attr)
            .and_then(|def| column_elem(&def.attr_type).ok());
        check_filter(&self.state, attr, filter)?;
        let nodes = select_nodes(&self.state, selector)?;
        // Every graph that holds this column — in a part or in the tail.
        let mut graphs: BTreeSet<u16> = self
            .columns
            .keys()
            .filter(|(_, a)| *a == attr_id.0)
            .map(|(g, _)| *g)
            .collect();
        for (node, staged_attr) in self.state.staging().keys() {
            if *staged_attr == attr_id.0 {
                graphs.insert(self.state.arrays().graph_of(NodeId(*node)).0);
            }
        }
        let id = self.next_cursor_id;
        self.next_cursor_id += 1;
        let now = (self.options.clock)();
        self.cursors.insert(
            id,
            Cursor {
                session,
                attr: attr_id,
                schema,
                nodes,
                filter: filter.clone(),
                graphs: graphs.into_iter().collect(),
                graph_pos: 0,
                elem,
                buffer: None,
                done_through: None,
                last_touch_us: now,
            },
        );
        Ok(id)
    }

    /// The cursor if `session` may use it and it is not idle-expired; it is
    /// taken out of the table for the duration of the call.
    fn take_cursor(&mut self, session: u64, id: u64) -> Result<Cursor, KernelError> {
        let now = (self.options.clock)();
        let Some(cursor) = self.cursors.remove(&id) else {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                format!(
                    "open cursors: {:?}",
                    self.cursors.keys().collect::<Vec<_>>()
                ),
                format!("cursor {id} is not open"),
            ));
        };
        if cursor.session != session {
            self.cursors.insert(id, cursor);
            return Err(KernelError::new(
                codes::NOT_FOUND,
                "",
                format!("cursor {id} belongs to another session"),
            ));
        }
        let idle = now.saturating_sub(cursor.last_touch_us);
        if idle > self.options.cursor_idle_ttl_us {
            // Expired: it stays gone.
            return Err(KernelError::new(
                codes::NOT_FOUND,
                "cursors expire after their idle ttl — reopen and page again",
                format!(
                    "cursor {id} expired after {idle} µs idle (ttl {} µs)",
                    self.options.cursor_idle_ttl_us
                ),
            ));
        }
        Ok(cursor)
    }

    fn cursor_fetch(
        &mut self,
        session: u64,
        id: u64,
        budget: Budget,
    ) -> Result<Batch, KernelError> {
        if budget.rows == 0 {
            return Err(KernelError::new(
                codes::INVALID_KEY,
                "budget.rows must be at least 1",
                "CURSOR_FETCH without a row budget",
            ));
        }
        let mut cursor = self.take_cursor(session, id)?;
        let outcome = self.stream(&mut cursor, budget);
        cursor.last_touch_us = (self.options.clock)();
        self.cursors.insert(id, cursor);
        match outcome {
            Ok(batch) => Ok(batch),
            Err(e) => Err(self.fatal(e)),
        }
    }

    /// Pull the next batch out of the column (parts ⊕ staging) in
    /// `(graph, node, k)` order. A node's cells are decoded ONCE per cursor
    /// and drained across fetches, whatever the page size; a run that
    /// liveness, selection or a filter excludes is never decoded.
    fn stream(&mut self, cursor: &mut Cursor, budget: Budget) -> Result<Batch, TxError> {
        let mut rows: Vec<CellRow> = Vec::new();
        let mut bytes = 0usize;
        let mut remaining = false;
        'fill: while cursor.graph_pos < cursor.graphs.len() {
            let graph = cursor.graphs[cursor.graph_pos];
            if cursor.buffer.is_none() {
                self.ensure_column(graph, cursor.attr)?;
                let part = self
                    .columns
                    .get(&(graph, cursor.attr.0))
                    .and_then(|id| self.parts.get(id));
                let after = cursor.done_through.map(|node| node.saturating_add(1));
                let mut reader = ColumnReader::new(
                    part,
                    self.state.staging(),
                    cursor.attr,
                    after,
                    cursor.elem.as_ref(),
                );
                loop {
                    // Pushdown: a run of a dead or unselected node, or one
                    // the filters exclude by directory / zone map, is left
                    // undecoded. (Staged cells are merged in regardless.)
                    let skip = |part: &Part, idx: usize| {
                        let run_node = part.run_node(idx).0;
                        !self.state.arrays().is_alive(NodeId(run_node))
                            || cursor
                                .nodes
                                .as_ref()
                                .is_some_and(|set| !set.contains(&run_node))
                            || run_excluded(part, idx, &cursor.filter)
                    };
                    let Some((node, cells)) = reader.next_run(skip)? else {
                        break;
                    };
                    // Staged cells arrive even when the part run was skipped:
                    // liveness and selection hold for them too.
                    let live = self.state.arrays().is_alive(NodeId(node));
                    let selected = cursor.nodes.as_ref().is_none_or(|set| set.contains(&node));
                    if !live || !selected || cells.is_empty() {
                        continue;
                    }
                    cursor.buffer = Some((node, cells, 0));
                    break;
                }
                if cursor.buffer.is_none() {
                    // This graph's column is exhausted.
                    cursor.graph_pos += 1;
                    cursor.done_through = None;
                    continue 'fill;
                }
            }
            let mut drained = None;
            if let Some((node, buffer, pos)) = cursor.buffer.as_mut() {
                while let Some((k, lsn, value)) = buffer.get(*pos).cloned() {
                    *pos += 1;
                    if !admits(&cursor.filter, k, &value) {
                        continue;
                    }
                    let size = value_bytes(&value);
                    let over = rows.len() >= budget.rows
                        || (!rows.is_empty() && bytes + size > budget.bytes);
                    if over {
                        // Not emitted: step back so the next fetch starts here.
                        *pos -= 1;
                        remaining = true;
                        break 'fill;
                    }
                    bytes += size;
                    rows.push(CellRow {
                        node: *node,
                        attr: cursor.attr,
                        k,
                        lsn,
                        value,
                    });
                }
                drained = Some(*node);
            }
            cursor.done_through = drained;
            cursor.buffer = None;
        }
        Ok(Batch {
            schema: cursor.schema.clone(),
            rows,
            remaining,
        })
    }

    fn cursor_close(&mut self, session: u64, id: u64) -> Result<(), KernelError> {
        self.take_cursor(session, id)?;
        Ok(())
    }

    /// The node's journal-derived history: every decoded record that names
    /// it, ascending by LSN, optionally inside an LSN range.
    fn history(
        &self,
        node: NodeId,
        range: Option<(u64, u64)>,
    ) -> Result<Vec<HistoryEvent>, TxError> {
        let mut decoded: Vec<(u64, Record)> = Vec::new();
        let mut failure: Option<TxError> = None;
        journal::recover(&journal_dir(&self.dir), |txn| {
            if failure.is_some() {
                return;
            }
            for rec in &txn.records {
                if rec.kind == RecordKind::Commit {
                    continue;
                }
                match Record::decode(rec.kind, &rec.payload) {
                    Ok(record) => decoded.push((rec.lsn, record)),
                    Err(e) => {
                        failure = Some(invariant(format!(
                            "journal record lsn {} ({:?}) does not decode: {e}",
                            rec.lsn, rec.kind
                        )))
                    }
                }
            }
        })?;
        if let Some(e) = failure {
            return Err(e);
        }
        if let Some((from, to)) = range {
            decoded.retain(|(lsn, _)| *lsn >= from && *lsn <= to);
        }
        Ok(history_events(node, &decoded, &|edge| {
            u32::try_from(edge)
                .ok()
                .and_then(|id| self.state.arrays().edge_slot(EdgeId(id)))
                .map(|r| (r.src.0, r.dst.0))
        }))
    }

    /// Runs decoded by the cached parts since open / last checkpoint — the
    /// zone-pushdown counter (SPEC-03 §4, SFT-4.3).
    pub fn parts_decoded_runs(&self) -> u64 {
        self.parts.values().map(Part::decoded_runs).sum()
    }

    /// `(graph, attr id) → part id` columns that currently live in a part.
    pub fn part_columns(&self) -> Vec<((u16, u16), u64)> {
        self.columns.iter().map(|(k, id)| (*k, *id)).collect()
    }

    /// Open cursor ids (tests, diagnostics).
    pub fn open_cursors(&self) -> Vec<u64> {
        self.cursors.keys().copied().collect()
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
    /// COMMIT LSN of the last txn applied.
    epoch: u64,
}

impl Recovery {
    /// One committed journal txn. Before the checkpoint the snapshot already
    /// holds the effects — only GUARD params need remembering; from the
    /// checkpoint on, every record replays through the live apply.
    fn on_txn(
        &mut self,
        txn: &JournalTxn,
        state: &mut EngineState,
        upto_lsn: Option<u64>,
    ) -> Result<(), TxError> {
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
        // A time-travel view stops at its epoch.
        if txn.last_lsn <= self.created_lsn || upto_lsn.is_some_and(|upto| txn.last_lsn > upto) {
            return Ok(());
        }
        for (rec, lsn) in decoded {
            state.apply(&rec, lsn)?;
        }
        self.epoch = txn.last_lsn;
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
fn load_parts(
    dir: &Path,
    manifest: &Manifest,
    state: &mut EngineState,
) -> Result<Vec<(u64, Part)>, TxError> {
    let mut parsed = Vec::with_capacity(manifest.parts.len());
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
        parsed.push((*id, part));
    }
    Ok(parsed)
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
/// Restore the logical type of an `I64`-column cell (SPEC-03 §4: parts store
/// Timestamp / Duration / NodeRef as I64; the attribute declares which).
pub(crate) fn retype(elem: &ElemType, value: Value) -> Result<Value, TxError> {
    match (elem, value) {
        (ElemType::Scalar(ScalarType::Timestamp), Value::I64(x)) => Ok(Value::Timestamp(x)),
        (ElemType::Scalar(ScalarType::Duration), Value::I64(x)) => Ok(Value::Duration(x)),
        (ElemType::Scalar(ScalarType::NodeRef), Value::I64(x)) => match u32::try_from(x) {
            Ok(id) => Ok(Value::NodeRef(NodeId(id))),
            Err(_) => Err(invariant(format!(
                "node_ref value {x} does not fit a node id"
            ))),
        },
        (_, other) => Ok(other),
    }
}

/// The column's element type (vector element, or the scalar/enum itself).
/// Records have no column element: they ride a Bytes column of core-codec
/// bytes.
pub(crate) fn column_elem(ty: &AttrType) -> Result<ElemType, ()> {
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

/// Can the part run be left UNdecoded? By key span (a dense run covers
/// `first_k ..< first_k + count`; a sparse run only bounds from below) and by
/// the zone maps. The node's staged cells are merged in regardless.
fn run_excluded(part: &Part, idx: usize, filter: &CursorFilter) -> bool {
    if let Some((lo, hi)) = filter.k
        && let Some((first_k, count)) = part.run_span(idx)
    {
        let last_k = first_k + u64::from(count.saturating_sub(1));
        if first_k > hi || (!part.sparse && last_k < lo) {
            return true;
        }
    }
    if let Some((lo, hi)) = &filter.value {
        return !part.run_admits_range(idx, lo, hi);
    }
    false
}

/// Order over comparable values (same variant); `None` when the two cannot
/// be compared.
fn value_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    Some(match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::I64(x), Value::I64(y))
        | (Value::Timestamp(x), Value::Timestamp(y))
        | (Value::Duration(x), Value::Duration(y)) => x.cmp(y),
        (Value::F64(x), Value::F64(y)) => x.partial_cmp(y)?,
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => x.unscaled.cmp(&y.unscaled),
        (Value::Enum(x), Value::Enum(y)) => x.cmp(y),
        (Value::NodeRef(x), Value::NodeRef(y)) => x.cmp(y),
        _ => return None,
    })
}

/// Does a decoded cell pass the cursor's filters?
fn admits(filter: &CursorFilter, k: u64, value: &Value) -> bool {
    if let Some((lo, hi)) = filter.k
        && !(lo..=hi).contains(&k)
    {
        return false;
    }
    if let Some((lo, hi)) = &filter.value {
        // Inclusive on both ends: `lo ≤ value` and `value ≤ hi`.
        let at_or_above_lo = value_cmp(lo, value).is_some_and(|o| o != std::cmp::Ordering::Greater);
        let at_or_below_hi = value_cmp(value, hi).is_some_and(|o| o != std::cmp::Ordering::Greater);
        return at_or_above_lo && at_or_below_hi;
    }
    true
}

/// A filter must be well-formed: closed ranges over the column's own type.
fn check_filter(state: &EngineState, attr: &Key, filter: &CursorFilter) -> Result<(), KernelError> {
    let bad = |detail: String| KernelError::new(codes::INVALID_KEY, "", detail);
    if let Some((lo, hi)) = filter.k
        && lo > hi
    {
        return Err(bad(format!("k range {lo}..{hi} is empty")));
    }
    if let Some((lo, hi)) = &filter.value {
        if value_cmp(lo, hi).is_none_or(|o| o == std::cmp::Ordering::Greater) {
            return Err(bad(format!(
                "value range {lo:?}..{hi:?} is empty or not ordered"
            )));
        }
        let def = state.catalog().attributes.get(attr).ok_or_else(|| {
            KernelError::new(
                codes::SCHEMA,
                "",
                format!("attribute `{attr}` is not declared"),
            )
        })?;
        let enums = &state.catalog().enums;
        let check = |v: &Value| match &def.attr_type {
            lattice_schema::entities::AttrType::Vector { elem } => {
                lattice_validate::Validators::r6_elem_type(elem, v, enums).err()
            }
            _ => {
                lattice_validate::Validators::r6_attr_type(def, v, enums, &state.catalog().records)
                    .err()
            }
        };
        for v in [lo, hi] {
            if let Some(violation) = check(v) {
                return Err(KernelError::new(codes::SCHEMA, "", violation.reason));
            }
        }
    }
    Ok(())
}
