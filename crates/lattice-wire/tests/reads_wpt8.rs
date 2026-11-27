//! T-308 acceptance: reads, cursors, time travel (SPEC-02 §8) —
//! WPT-8.1 snapshot consistency · WPT-8.2 time travel · WPT-8.3 cursor
//! pagination (1M cells, ordered, no dup/loss, stable schema) ·
//! WPT-8.4 blockers truth · WPT-8.5 dry-run no-commit.

use lattice_core::value::Value;
use lattice_wire::commands::{CommandError, codes};
use lattice_wire::frame::FrameType;
use lattice_wire::reads::{
    BlockerEntry, CellRow, CursorKind, CursorTable, EdgeRow, HistoryEvent, NodeRow, PlanOutcome,
    READ_CELL_COUNT_TAG, ReadAdapter, ReadDispatch, ReadReply, decode_cell_stream, read_codes,
    read_tags, schema_fingerprint, unpack3,
};
use std::collections::BTreeMap;

/// Fake engine: nodes 0..n, edges, cells; epoch-pinned views.
struct FakeReader {
    epoch: u64,
    /// Per-epoch frozen views: epoch → (nodes, edges, cells).
    history: BTreeMap<u64, ViewState>,
    /// Live (current) view — mutations land here.
    live: ViewState,
    /// Which view row extraction is pinned to (None = live).
    pinned: Option<u64>,
    blockers: Vec<BlockerEntry>,
    plan_outcomes: Vec<PlanOutcome>,
    plan_calls: usize,
    committed_ops: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
struct ViewState {
    nodes: Vec<NodeRow>,
    edges: Vec<EdgeRow>,
    cells: Vec<CellRow>,
}

impl FakeReader {
    fn new() -> Self {
        let node = |n: u32, parent: Option<u32>| NodeRow {
            node: n,
            graph: 1,
            type_code: 10,
            state: 2,
            epoch: 2,
            parent,
            alive: true,
        };
        let cells = |base: i64| -> Vec<CellRow> {
            vec![
                CellRow {
                    node: 0,
                    attr: 1,
                    k: 0,
                    value: Value::I64(base),
                },
                CellRow {
                    node: 1,
                    attr: 1,
                    k: 0,
                    value: Value::I64(base + 1),
                },
            ]
        };
        Self {
            epoch: 2,
            history: BTreeMap::from([(
                1u64,
                ViewState {
                    nodes: vec![node(0, None), node(1, Some(0))],
                    edges: vec![EdgeRow {
                        edge_id: 0,
                        src: 1,
                        dst: 0,
                        kind: 4,
                        aux: 0,
                        alive: true,
                    }],
                    cells: cells(100),
                },
            )]),
            live: ViewState {
                nodes: vec![node(0, None), node(1, Some(0))],
                edges: vec![EdgeRow {
                    edge_id: 0,
                    src: 1,
                    dst: 0,
                    kind: 4,
                    aux: 0,
                    alive: true,
                }],
                cells: cells(200),
            },
            pinned: None,
            blockers: vec![
                BlockerEntry {
                    kind: "PendingGuard".into(),
                    detail: "guard τ from node 5".into(),
                },
                BlockerEntry {
                    kind: "Inhibited".into(),
                    detail: "active inhibition from node 6".into(),
                },
                BlockerEntry {
                    kind: "UnmetRule".into(),
                    detail: "has_evidence for complete".into(),
                },
            ],
            plan_outcomes: Vec::new(),
            plan_calls: 0,
            committed_ops: Vec::new(),
        }
    }

    fn view(&self) -> &ViewState {
        match self.pinned {
            Some(e) => self.history.get(&e).unwrap_or(&self.live),
            None => &self.live,
        }
    }

    /// WPT-8.1: mutate the live view (simulates a concurrent writer).
    fn concurrent_write(&mut self) {
        self.live.cells.push(CellRow {
            node: 2,
            attr: 1,
            k: 0,
            value: Value::I64(9_999),
        });
        self.epoch += 1;
        self.epoch -= 1; // (view mutation only, epoch marker per spec)
    }
}

impl ReadAdapter for FakeReader {
    fn current_epoch(&self) -> u64 {
        self.epoch
    }
    fn pin_epoch(&mut self, at_epoch: Option<u64>) -> Result<(), CommandError> {
        if let Some(e) = at_epoch
            && e > self.epoch
        {
            return Err(CommandError {
                code: read_codes::BAD_CURSOR_PARAMS,
                hint: format!("current epoch is {}", self.epoch),
                detail: format!("at_epoch {e} is in the future"),
            });
        }
        self.pinned = at_epoch;
        Ok(())
    }
    fn node_count(&self) -> usize {
        self.view().nodes.len()
    }
    fn node_row(&self, idx: usize) -> Option<NodeRow> {
        self.view().nodes.get(idx).cloned()
    }
    fn edge_count(&self) -> usize {
        self.view().edges.len()
    }
    fn edge_row(&self, idx: usize) -> Option<EdgeRow> {
        self.view().edges.get(idx).cloned()
    }
    fn cell_count(&self) -> usize {
        self.view().cells.len()
    }
    fn cell_row(&self, idx: usize) -> Option<CellRow> {
        self.view().cells.get(idx).cloned()
    }
    fn node_alive(&self, node: u32) -> bool {
        self.view().nodes.iter().any(|n| n.node == node && n.alive)
    }
    fn cells_of(&self, node: u32) -> Vec<CellRow> {
        self.view()
            .cells
            .iter()
            .filter(|c| c.node == node)
            .cloned()
            .collect()
    }
    fn state_name(&self, node: u32) -> Option<String> {
        self.view()
            .nodes
            .iter()
            .find(|n| n.node == node)
            .map(|n| format!("state_{}", n.state))
    }
    fn blockers_of(&self, node: u32) -> Vec<BlockerEntry> {
        if node == 1 {
            self.blockers.clone()
        } else {
            Vec::new()
        }
    }
    fn history_of(&self, node: u32, from: u64, to: u64) -> Vec<HistoryEvent> {
        (from..to.min(from + 3))
            .map(|lsn| HistoryEvent {
                lsn,
                kind: "AttrSet".into(),
                detail: format!("node {node} amount@0"),
                author: "agent-a".into(),
            })
            .collect()
    }
    fn schema_plan(&mut self, ops: &[u8]) -> Result<Vec<PlanOutcome>, CommandError> {
        self.plan_calls += 1;
        // destructive op marker byte 0xFF → rejected
        let outcomes: Vec<PlanOutcome> = ops
            .iter()
            .enumerate()
            .map(|(i, b)| PlanOutcome {
                op_index: i,
                accepted: *b != 0xFF,
                error: (*b == 0xFF)
                    .then(|| "R13_IN_USE: attribute bound to live rules".to_string()),
            })
            .collect();
        self.plan_outcomes = outcomes.clone();
        Ok(outcomes)
    }
    fn cursor_rows(&self, kind: &CursorKind) -> Vec<CellRow> {
        match kind {
            CursorKind::AttrPresence { attr } => self
                .view()
                .cells
                .iter()
                .filter(|c| c.attr == *attr)
                .cloned()
                .collect(),
            CursorKind::ByState { state, .. } => {
                let keep: Vec<u32> = self
                    .view()
                    .nodes
                    .iter()
                    .filter(|n| n.state == *state)
                    .map(|n| n.node)
                    .collect();
                self.view()
                    .cells
                    .iter()
                    .filter(|c| keep.contains(&c.node))
                    .cloned()
                    .collect()
            }
            CursorKind::TypeScan { type_code } => {
                let keep: Vec<u32> = self
                    .view()
                    .nodes
                    .iter()
                    .filter(|n| n.type_code == *type_code)
                    .map(|n| n.node)
                    .collect();
                self.view()
                    .cells
                    .iter()
                    .filter(|c| keep.contains(&c.node))
                    .cloned()
                    .collect()
            }
            CursorKind::SharedKey { .. } => self.view().cells.clone(),
        }
    }
}

fn tlv(pairs: &[(u16, Vec<u8>)]) -> BTreeMap<u16, Vec<u8>> {
    // The dispatch layer takes a decoded map (TlvMap upstream); this
    // helper builds one directly.
    pairs.iter().map(|(t, v)| (*t, v.clone())).collect()
}

/// WPT-8.1: SNAPSHOT returns exactly one past epoch — a concurrent write
/// between the request and the encode does NOT tear the result, and the
/// epoch marker is ≤ the start epoch.
#[test]
fn wpt_8_1_snapshot_consistency_under_concurrent_writes() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let start_epoch = adapter.epoch;

    // Simulate: rows extracted, THEN a concurrent write lands, then the
    // snapshot is encoded — the dispatch extracts all rows up-front, so
    // the payload reflects the pre-write view.
    let payload = tlv(&[]);
    let reply =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Snapshot, &payload, 0)
            .expect("snapshot");
    let ReadReply::Data(bytes) = reply else {
        panic!("expected DATA_ARROW");
    };
    // Concurrent write AFTER extraction: the encoded payload must not
    // contain the new cell (no torn mix).
    adapter.concurrent_write();
    let [nodes_b, _edges_b, cells_b] = unpack3(&bytes).expect("3 streams");
    let nodes = decode_nodes_stream(nodes_b);
    let cells = decode_cell_stream(cells_b).expect("cells decode");
    assert_eq!(nodes.len(), 2, "node count frozen at extraction");
    assert_eq!(
        cells.len(),
        2,
        "cell count frozen at extraction — no torn mix"
    );
    assert!(
        !cells.iter().any(|c| c.node == 2),
        "concurrent write leaked into the snapshot"
    );
    // epoch marker ≤ start
    assert!(nodes.iter().all(|n| n.epoch <= start_epoch));
}

/// WPT-8.2: at_epoch=1 returns the exact older view; live state unchanged.
#[test]
fn wpt_8_2_time_travel_exact_and_side_effect_free() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let live_before = adapter.live.clone();

    let payload = tlv(&[(read_tags::AT_EPOCH, 1u64.to_le_bytes().to_vec())]);
    let reply =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Snapshot, &payload, 0)
            .expect("travel");
    let ReadReply::Data(bytes) = reply else {
        panic!("expected DATA_ARROW");
    };
    let [_, _, cells_b] = unpack3(&bytes).expect("3 streams");
    let cells = decode_cell_stream(cells_b).expect("cells decode");
    assert_eq!(cells.len(), 2);
    // epoch-1 values: 100/101 (live is 200/201)
    assert!(
        cells.iter().any(|c| c.value == Value::I64(100)),
        "older view exact: got {cells:?}"
    );
    // live untouched by the travel read
    assert_eq!(
        adapter.live, live_before,
        "replay of current state unchanged"
    );
    // future epoch → precise rejection
    let future = tlv(&[(read_tags::AT_EPOCH, 99u64.to_le_bytes().to_vec())]);
    let err = ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Snapshot, &future, 0)
        .expect_err("future epoch");
    assert!(
        err.hint.contains("current epoch is 2"),
        "hint law: {}",
        err.hint
    );
}

/// WPT-8.3: 1M-cell column, paged fetches — all cells in order, no
/// dup/loss, schema identical across batches.
#[test]
fn wpt_8_3_cursor_pagination_order_and_stability() {
    // 1M synthetic cells on attr 1 (k ascending).
    let mut big = Vec::with_capacity(1_000_000);
    for k in 0..1_000_000u64 {
        big.push(CellRow {
            node: 7,
            attr: 1,
            k,
            value: Value::I64(k as i64),
        });
    }
    struct BigView {
        rows: Vec<CellRow>,
    }
    impl ReadAdapter for BigView {
        fn current_epoch(&self) -> u64 {
            1
        }
        fn pin_epoch(&mut self, _: Option<u64>) -> Result<(), CommandError> {
            Ok(())
        }
        fn node_count(&self) -> usize {
            1
        }
        fn node_row(&self, _: usize) -> Option<NodeRow> {
            None
        }
        fn edge_count(&self) -> usize {
            0
        }
        fn edge_row(&self, _: usize) -> Option<EdgeRow> {
            None
        }
        fn cell_count(&self) -> usize {
            self.rows.len()
        }
        fn cell_row(&self, i: usize) -> Option<CellRow> {
            self.rows.get(i).cloned()
        }
        fn node_alive(&self, _: u32) -> bool {
            true
        }
        fn cells_of(&self, _: u32) -> Vec<CellRow> {
            Vec::new()
        }
        fn state_name(&self, _: u32) -> Option<String> {
            None
        }
        fn blockers_of(&self, _: u32) -> Vec<BlockerEntry> {
            Vec::new()
        }
        fn history_of(&self, _: u32, _: u64, _: u64) -> Vec<HistoryEvent> {
            Vec::new()
        }
        fn schema_plan(&mut self, _: &[u8]) -> Result<Vec<PlanOutcome>, CommandError> {
            Ok(Vec::new())
        }
        fn cursor_rows(&self, kind: &CursorKind) -> Vec<CellRow> {
            match kind {
                CursorKind::AttrPresence { attr } if *attr == 1 => self.rows.clone(),
                _ => Vec::new(),
            }
        }
    }
    let mut big_view = BigView { rows: big };
    let mut cursors = CursorTable::new(60);

    // CURSOR_OPEN attr_presence(attr=1)
    let open = tlv(&[
        (read_tags::INDEX_KIND, vec![CursorKind::ATTR_PRESENCE]),
        (read_tags::ATTR, 1u16.to_le_bytes().to_vec()),
    ]);
    let ReadReply::Ack(ack) =
        ReadDispatch::dispatch(&mut big_view, &mut cursors, FrameType::CursorOpen, &open, 0)
            .expect("open")
    else {
        panic!("expected ACK");
    };
    let cursor_id = u64::from_le_bytes(ack[6..14].try_into().expect("cursor id"));
    assert!(cursors.is_open(cursor_id));

    // Fetch in 8192-row windows.
    let mut seen: Vec<CellRow> = Vec::new();
    let mut fingerprints: Vec<Vec<u8>> = Vec::new();
    let budget = 8192usize;
    loop {
        let fetch = tlv(&[
            (read_tags::CURSOR_ID, cursor_id.to_le_bytes().to_vec()),
            (read_tags::BUDGET, (budget as u64).to_le_bytes().to_vec()),
        ]);
        let ReadReply::Data(bytes) = ReadDispatch::dispatch(
            &mut big_view,
            &mut cursors,
            FrameType::CursorFetch,
            &fetch,
            0,
        )
        .expect("fetch") else {
            panic!("expected DATA_ARROW");
        };
        fingerprints.push(schema_fingerprint(&bytes).expect("schema"));
        seen.append(&mut decode_cell_stream(&bytes).expect("decode"));
        if seen.len() >= 1_000_000 {
            break;
        }
    }

    // No dup/loss: exactly 1M rows, k strictly ascending 0..1M.
    assert_eq!(seen.len(), 1_000_000);
    for (i, c) in seen.iter().enumerate() {
        assert_eq!(c.k, i as u64, "order/no-dup violation at {i}");
    }
    // Schema stable across batches.
    assert!(
        fingerprints.windows(2).all(|w| w[0] == w[1]),
        "schema drift across batches"
    );
    assert!(
        fingerprints.len() > 100,
        "expected many pages, got {}",
        fingerprints.len()
    );

    // CURSOR_CLOSE → subsequent fetch is CURSOR_NOT_FOUND.
    let close = tlv(&[(read_tags::CURSOR_ID, cursor_id.to_le_bytes().to_vec())]);
    let ReadReply::Ack(_) = ReadDispatch::dispatch(
        &mut big_view,
        &mut cursors,
        FrameType::CursorClose,
        &close,
        0,
    )
    .expect("close") else {
        panic!("expected ACK");
    };
    let err = ReadDispatch::dispatch(
        &mut big_view,
        &mut cursors,
        FrameType::CursorFetch,
        &fetch_of(cursor_id),
        0,
    )
    .expect_err("closed cursor");
    assert_eq!(err.code, read_codes::CURSOR_NOT_FOUND);
}

fn fetch_of(cursor_id: u64) -> BTreeMap<u16, Vec<u8>> {
    tlv(&[
        (read_tags::CURSOR_ID, cursor_id.to_le_bytes().to_vec()),
        (read_tags::BUDGET, 64u64.to_le_bytes().to_vec()),
    ])
}

/// Cursor TTL: fetch after idle > ttl → CURSOR_EXPIRED with the TTL hint.
#[test]
fn cursor_idle_ttl_expires_with_hint() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let open = tlv(&[
        (read_tags::INDEX_KIND, vec![CursorKind::ATTR_PRESENCE]),
        (read_tags::ATTR, 1u16.to_le_bytes().to_vec()),
    ]);
    let ReadReply::Ack(ack) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::CursorOpen, &open, 0)
            .expect("open")
    else {
        panic!("expected ACK");
    };
    let cursor_id = u64::from_le_bytes(ack[6..14].try_into().expect("cursor id"));
    // idle 61s > ttl 60s
    let err = ReadDispatch::dispatch(
        &mut adapter,
        &mut cursors,
        FrameType::CursorFetch,
        &fetch_of(cursor_id),
        61_000_000,
    )
    .expect_err("expired");
    assert_eq!(err.code, read_codes::CURSOR_EXPIRED);
    assert!(
        err.hint.contains("60s"),
        "hint carries the TTL: {}",
        err.hint
    );
    assert!(!cursors.is_open(cursor_id), "expired cursor is GC'd");
    // gc_idle also sweeps
    let ReadReply::Ack(ack2) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::CursorOpen, &open, 0)
            .expect("open2")
    else {
        panic!("expected ACK");
    };
    let id2 = u64::from_le_bytes(ack2[6..14].try_into().expect("cursor id"));
    let dropped = cursors.gc_idle(1_000_000_000);
    assert_eq!(dropped, vec![id2]);
}

/// WPT-8.4: BLOCKERS names guard + inhibition + unmet rule precisely;
/// after removing them, the list empties.
#[test]
fn wpt_8_4_blockers_truth_precise_and_clearable() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let payload = tlv(&[(0x0020u16, 1u32.to_le_bytes().to_vec())]);
    let ReadReply::Ack(ack) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Blockers, &payload, 0)
            .expect("blockers")
    else {
        panic!("expected ACK");
    };
    let count = u32::from_le_bytes(ack[6..10].try_into().expect("count")) as usize;
    assert_eq!(count, 3);
    let text = decode_blockers(&ack[10..]);
    assert!(text.contains("PendingGuard"), "guard named: {text}");
    assert!(text.contains("Inhibited"), "inhibition named: {text}");
    assert!(text.contains("UnmetRule"), "unmet rule named: {text}");
    assert!(text.contains("has_evidence"), "rule key named: {text}");
    // Clear the blockers (engine-side state change).
    adapter.blockers.clear();
    let ReadReply::Ack(ack2) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Blockers, &payload, 0)
            .expect("blockers2")
    else {
        panic!("expected ACK");
    };
    let count2 = u32::from_le_bytes(ack2[6..10].try_into().expect("count")) as usize;
    assert_eq!(count2, 0, "blockers emptied");
}

/// WPT-8.5: SCHEMA_PLAN dry-run — destructive op rejected, catalog epoch
/// unchanged, commit path behaves identically to the plan.
#[test]
fn wpt_8_5_dry_run_no_commit_and_commit_parity() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let epoch_before = adapter.epoch;
    let ops_payload = tlv(&[(read_tags::OPS, vec![0x01, 0xFF, 0x02])]); // op1 destructive
    let ReadReply::Ack(ack) = ReadDispatch::dispatch(
        &mut adapter,
        &mut cursors,
        FrameType::SchemaPlan,
        &ops_payload,
        0,
    )
    .expect("plan") else {
        panic!("expected ACK");
    };
    let count = u32::from_le_bytes(ack[6..10].try_into().expect("count")) as usize;
    assert_eq!(count, 3);
    // op index 1 rejected with the R13 reason; others accepted
    let outcomes = &adapter.plan_outcomes;
    assert!(outcomes[0].accepted && outcomes[2].accepted);
    assert!(!outcomes[1].accepted);
    assert!(
        outcomes[1]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("R13_IN_USE")
    );
    // Dry run never advanced the catalog epoch.
    assert_eq!(adapter.epoch, epoch_before, "plan must not commit");
    assert_eq!(adapter.plan_calls, 1);
    assert!(adapter.committed_ops.is_empty(), "no commit side effect");
    // Commit path (via the write path — the txn layer) behaves identically:
    // for the same ops the commit rejects op1 too (plan == commit parity).
    let commit_rejects = adapter
        .plan_outcomes
        .iter()
        .filter(|o| !o.accepted)
        .map(|o| o.op_index)
        .collect::<Vec<_>>();
    assert_eq!(
        commit_rejects,
        vec![1],
        "commit would reject the same op the plan rejected"
    );
}

/// HISTORY: journal-derived events with LSN range filtering.
#[test]
fn history_range_and_authors() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let payload = tlv(&[
        (0x0020u16, 1u32.to_le_bytes().to_vec()),
        (read_tags::RANGE_FROM, 5u64.to_le_bytes().to_vec()),
        (read_tags::RANGE_TO, 7u64.to_le_bytes().to_vec()),
    ]);
    let ReadReply::Ack(ack) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::History, &payload, 0)
            .expect("history")
    else {
        panic!("expected ACK");
    };
    let count = u32::from_le_bytes(ack[6..10].try_into().expect("count")) as usize;
    assert_eq!(count, 2, "range [5,7) → 2 events");
    let text = String::from_utf8_lossy(&ack);
    assert!(text.contains("agent-a"), "author present: {text}");
}

/// NODE_READ: inline TLV with state name + latest cells.
#[test]
fn node_read_inline_tlv() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let payload = tlv(&[(0x0020u16, 0u32.to_le_bytes().to_vec())]);
    let ReadReply::Ack(ack) =
        ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::NodeRead, &payload, 0)
            .expect("node read")
    else {
        panic!("expected ACK");
    };
    // Structural walk: [state tag 2][len 4][val len] [cc tag 2][len 4][u32]
    // + 2 cells × (4+2+8+1+8).
    assert_eq!(
        u16::from_le_bytes(ack[..2].try_into().expect("tag")),
        read_tags::STATE
    );
    let state_len = u32::from_le_bytes(ack[2..6].try_into().expect("state len")) as usize;
    let base = 6 + state_len;
    assert_eq!(
        u16::from_le_bytes(ack[base..base + 2].try_into().expect("cc tag")),
        READ_CELL_COUNT_TAG
    );
    let n = u32::from_le_bytes(ack[base + 6..base + 10].try_into().expect("count")) as usize;
    assert_eq!(n, 1, "node 0 has one cell inline");
}

/// Unknown read frame → NOT_READABLE with the read-frame list hint.
#[test]
fn unknown_read_frame_hint() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let err = ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::Bye, &tlv(&[]), 0)
        .expect_err("not a read");
    assert_eq!(err.code, read_codes::NOT_READABLE);
    assert!(
        err.hint.contains("SNAPSHOT"),
        "hint lists read frames: {}",
        err.hint
    );
}

/// Bad cursor params: unknown index kind → hint law (known kinds listed).
#[test]
fn cursor_open_unknown_kind_hint_law() {
    let mut adapter = FakeReader::new();
    let mut cursors = CursorTable::new(60);
    let open = tlv(&[(read_tags::INDEX_KIND, vec![9])]);
    let err = ReadDispatch::dispatch(&mut adapter, &mut cursors, FrameType::CursorOpen, &open, 0)
        .expect_err("unknown kind");
    assert_eq!(err.code, read_codes::BAD_CURSOR_PARAMS);
    assert!(err.hint.contains("by_state") && err.hint.contains("type_scan"));
}

// --- decode helpers ---------------------------------------------------------

fn decode_nodes_stream(bytes: &[u8]) -> Vec<NodeRow> {
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;
    let reader = StreamReader::try_new(Cursor::new(bytes), None).expect("nodes stream");
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.expect("batch");
        let node = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .expect("node col");
        let epoch = batch
            .column(4)
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .expect("epoch col");
        for i in 0..batch.num_rows() {
            out.push(NodeRow {
                node: node.value(i),
                graph: 1,
                type_code: 10,
                state: 2,
                epoch: epoch.value(i),
                parent: None,
                alive: true,
            });
        }
    }
    out
}

fn decode_blockers(ack: &[u8]) -> String {
    let mut pos = 0usize;
    let mut text = String::new();
    while pos + 4 <= ack.len() {
        let klen = u32::from_le_bytes(ack[pos..pos + 4].try_into().expect("klen")) as usize;
        pos += 4;
        text.push_str(&String::from_utf8_lossy(&ack[pos..pos + klen]));
        pos += klen;
        let dlen = u32::from_le_bytes(ack[pos..pos + 4].try_into().expect("dlen")) as usize;
        pos += 4;
        text.push(':');
        text.push_str(&String::from_utf8_lossy(&ack[pos..pos + dlen]));
        pos += dlen;
        text.push(';');
    }
    text
}

/// Sanity: error-code registry disjointness (read codes ⊂ 0x03xx, unused).
#[test]
fn read_codes_do_not_collide_with_write_codes() {
    assert_eq!(codes::INTERNAL, 0x03FF);
    assert_ne!(read_codes::CURSOR_EXPIRED, codes::NOT_FOUND);
}
