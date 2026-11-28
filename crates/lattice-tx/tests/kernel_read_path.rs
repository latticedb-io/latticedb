//! T-314 acceptance — the kernel read path (ARCHITECTURE §5.2, SPEC-02 §8)
//! over a real database directory:
//!
//! - Parity invariant: a column read (parts ⊕ staging) equals the engine's
//!   cell store — tail only, parts only, parts + tail, and after a reopen.
//! - WPT-8.1 — a snapshot under concurrent writers is exactly one committed
//!   epoch (no torn mix); the epoch marker never runs ahead.
//! - WPT-8.2 — time travel returns the exact older view and leaves the live
//!   state untouched.
//! - WPT-8.3 — a cursor pages a 1M-cell column in order, no dup/loss, with
//!   one schema across batches.
//! - WPT-8.4 — BLOCKERS names guard, inhibition and unmet rule precisely,
//!   and empties once they are gone.
//! - SGT-6.2 — the latest cell of a 10k-cell node is read by decoding ONE
//!   run, whatever else the column holds.
//! - SFT-4.3 shape — runs outside a value range are never decoded.

mod common;

use common::*;
use lattice_core::ids::NodeId;
use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_engine::dispatch::kinds;
use lattice_schema::entities::ActorClass;
use lattice_tx::{
    Budget, CellRow, Command, CursorFilter, GraphView, Kernel, KernelHandle, Read, ReadReply,
    Reply, Row, Selector, SubtreeLockTable, codes, shutdown_shared,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A byte budget that never binds (paging is driven by rows).
const ANY_BYTES: usize = usize::MAX;

fn create(
    kernel: &mut Kernel,
    node_type: &str,
    parent: Option<u32>,
    attrs: Vec<(&str, Value)>,
) -> NodeId {
    match kernel
        .execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("pm"),
                node_type: key(node_type),
                parent: parent.map(NodeId),
                attrs: attrs.into_iter().map(|(k, v)| (key(k), v)).collect(),
            },
        )
        .expect("create node")
    {
        Reply::Created(id) => id,
        other => panic!("expected Created, got {other:?}"),
    }
}

fn set(kernel: &mut Kernel, node: NodeId, attr: &str, value: Value) {
    kernel
        .execute(
            0,
            &subject(),
            &Command::AttrSet {
                node,
                attr: key(attr),
                value,
            },
        )
        .expect("attr set");
}

/// Append `values` to the node's `log` vector, one row each.
fn log_rows(kernel: &mut Kernel, node: NodeId, values: &[i64]) {
    kernel
        .execute(
            0,
            &subject(),
            &Command::RowAppend {
                node,
                set: key("task_table"),
                rows: values
                    .iter()
                    .map(|v| Row {
                        k_skip: 0,
                        cells: vec![(key("log"), Value::I64(*v))],
                    })
                    .collect(),
            },
        )
        .expect("row append");
}

fn read(kernel: &mut Kernel, read: &Read) -> ReadReply {
    kernel.read(0, read).expect("read succeeds")
}

fn snapshot(kernel: &mut Kernel) -> GraphView {
    match read(
        kernel,
        &Read::Snapshot {
            graph: None,
            subtree: None,
            at_epoch: None,
        },
    ) {
        ReadReply::Snapshot(view) => view,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

/// Page a cursor to the end; returns the batches and the concatenated rows.
fn drain(
    kernel: &mut Kernel,
    attr: &str,
    selector: Selector,
    filter: CursorFilter,
    budget: Budget,
) -> (Vec<String>, Vec<CellRow>) {
    let cursor = match read(
        kernel,
        &Read::CursorOpen {
            attr: key(attr),
            selector,
            filter,
        },
    ) {
        ReadReply::Cursor(id) => id,
        other => panic!("expected a cursor id, got {other:?}"),
    };
    let mut schemas = Vec::new();
    let mut rows = Vec::new();
    loop {
        match read(kernel, &Read::CursorFetch { cursor, budget }) {
            ReadReply::Batch(batch) => {
                schemas.push(batch.schema.fingerprint());
                assert!(
                    batch
                        .rows
                        .windows(2)
                        .all(|w| (w[0].node, w[0].k) < (w[1].node, w[1].k)),
                    "batch ascending"
                );
                rows.extend(batch.rows);
                if !batch.remaining {
                    break;
                }
            }
            other => panic!("expected a batch, got {other:?}"),
        }
    }
    read(kernel, &Read::CursorClose { cursor });
    (schemas, rows)
}

fn attr_id(kernel: &Kernel, attr: &str) -> u16 {
    kernel.state().machine().compiled_ref().attr_ids[&key(attr)].0
}

/// `(node, k, lsn, value)` of every cell the engine holds for an attribute.
fn engine_cells(kernel: &Kernel, attr: &str) -> BTreeSet<(u32, u64, u64, String)> {
    let id = lattice_core::ids::AttrId(attr_id(kernel, attr));
    kernel
        .state()
        .machine()
        .cells()
        .filter(|((_, a, _), _)| *a == id)
        .map(|((n, _, k), c)| (n.0, k.0, c.lsn.0, format!("{:?}", c.value)))
        .collect()
}

fn cursor_cells(kernel: &mut Kernel, attr: &str, page_rows: usize) -> Vec<CellRow> {
    drain(
        kernel,
        attr,
        Selector::All,
        CursorFilter::default(),
        Budget {
            rows: page_rows,
            bytes: ANY_BYTES,
        },
    )
    .1
}

// -- parity ---------------------------------------------------------------------

/// Whatever the engine holds for a column is exactly what a cursor returns —
/// from the staging tail alone, from parts alone, from both, and after a
/// reopen. Pages of 3 rows force resume at every kind of boundary.
#[test]
fn column_reads_equal_the_engine_store_in_every_layer_state() {
    const ATTRS: [&str; 11] = [
        "title", "amount", "owner", "evidence", "prio", "budget", "due_at", "sla", "blob", "log",
        "reading",
    ];
    let dir = test_dir("parity");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let mut tasks = Vec::new();
    for i in 0..6i64 {
        let task = create(
            &mut kernel,
            "task",
            Some(project.0),
            vec![("title", Value::str(format!("t{i}")).unwrap())],
        );
        set(&mut kernel, task, "amount", Value::I64(i * 3 + 1));
        set(&mut kernel, task, "prio", Value::Enum((i % 2) as u8));
        set(
            &mut kernel,
            task,
            "budget",
            Value::Decimal(Decimal::new(i128::from(i), 10, 2).unwrap()),
        );
        set(
            &mut kernel,
            task,
            "due_at",
            Value::Timestamp(1_700_000_000_000_000 + i),
        );
        set(&mut kernel, task, "owner", Value::NodeRef(project));
        set(
            &mut kernel,
            task,
            "evidence",
            Value::Record(
                RecordValue::from_pairs(&[(0, Value::I64(i)), (1, Value::str("n").unwrap())])
                    .unwrap(),
            ),
        );
        log_rows(
            &mut kernel,
            task,
            &(0..=i % 4).map(|j| i * 100 + j).collect::<Vec<_>>(),
        );
        tasks.push(task);
    }

    let check = |kernel: &mut Kernel, phase: &str| {
        for attr in ATTRS {
            let got: BTreeSet<(u32, u64, u64, String)> = cursor_cells(kernel, attr, 3)
                .iter()
                .map(|r| (r.node, r.k, r.lsn, format!("{:?}", r.value)))
                .collect();
            assert_eq!(got, engine_cells(kernel, attr), "{phase}: column `{attr}`");
            assert_eq!(
                got.len(),
                cursor_cells(kernel, attr, 1_000).len(),
                "{phase}: `{attr}` — page size does not change the result"
            );
        }
    };
    check(&mut kernel, "staging tail only");

    kernel.checkpoint().expect("checkpoint");
    assert!(
        kernel.state().staging().is_empty(),
        "the tail moved into parts"
    );
    check(&mut kernel, "parts only");

    // More work on top of the parts: appends, overwrites, a new node, a delete.
    log_rows(&mut kernel, tasks[2], &[999, 1000]);
    set(
        &mut kernel,
        tasks[0],
        "title",
        Value::str("renamed").unwrap(),
    );
    let late = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("late").unwrap())],
    );
    log_rows(&mut kernel, late, &[7, 8, 9]);
    kernel
        .execute(
            0,
            &subject(),
            &Command::DeleteNode {
                node: tasks[4],
                cascade: true,
            },
        )
        .expect("delete");
    check(&mut kernel, "parts + tail");

    kernel.close().expect("close");
    let (mut reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    check(&mut reopened, "reopened");
}

// -- WPT-8.1 --------------------------------------------------------------------

/// A project with `tasks` tasks titled `task {i}`.
fn populated(name: &str, tasks: usize) -> (std::path::PathBuf, Kernel, NodeId, Vec<NodeId>) {
    let dir = test_dir(name);
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let nodes = (0..tasks)
        .map(|i| {
            create(
                &mut kernel,
                "task",
                Some(project.0),
                vec![("title", Value::str(format!("task {i}")).unwrap())],
            )
        })
        .collect();
    (dir, kernel, project, nodes)
}

fn scalar(view: &GraphView, node: u32, attr: u16) -> Option<&Value> {
    view.scalars
        .iter()
        .find(|c| c.node == node && c.attr.0 == attr)
        .map(|c| &c.value)
}

/// WPT-8.1: four writers each drive three tasks through 40 rounds of
/// "title = r{n}, then amount = n + 1" while the test thread keeps taking
/// snapshots. Every snapshot is exactly one committed epoch: each task shows
/// either both writes of a round or only its title — never a mix that skips
/// ahead — and the epoch marker never decreases nor runs past the journal.
#[test]
fn wpt_8_1_snapshot_is_one_committed_epoch_under_concurrent_writers() {
    const ROUNDS: u64 = 40;
    let (_dir, kernel, _project, tasks) = populated("wpt_8_1", 12);
    let title_id = attr_id(&kernel, "title");
    let amount_id = attr_id(&kernel, "amount");
    let expected_nodes: BTreeSet<u32> = tasks.iter().map(|n| n.0).chain([_project.0]).collect();

    let handle = Arc::new(KernelHandle::spawn(kernel, 64).expect("sequencer"));
    let locks = Arc::new(SubtreeLockTable::new());
    let mut writers = Vec::new();
    for chunk in tasks.chunks(3) {
        let handle = Arc::clone(&handle);
        let locks = Arc::clone(&locks);
        let chunk = chunk.to_vec();
        writers.push(std::thread::spawn(move || {
            for round in 0..ROUNDS {
                for task in &chunk {
                    // Title first, then amount: holding the node's U-lock
                    // across both keeps a task's rounds in order.
                    let _guard = locks.acquire(vec![*task]).expect("lock");
                    for (attr, value) in [
                        ("title", Value::str(format!("r{round}")).unwrap()),
                        ("amount", Value::I64(round as i64 + 1)),
                    ] {
                        let cmd = Command::AttrSet {
                            node: *task,
                            attr: key(attr),
                            value,
                        };
                        handle
                            .submit(move |kernel| kernel.execute(0, &subject(), &cmd))
                            .expect("submit")
                            .recv()
                            .expect("receipt")
                            .expect("write accepted");
                    }
                }
            }
        }));
    }

    let mut views = 0usize;
    let mut last_marker = 0u64;
    let mut markers = Vec::new();
    while writers.iter().any(|w| !w.is_finished()) {
        let view = match handle
            .submit(|kernel| {
                kernel.read(
                    0,
                    &Read::Snapshot {
                        graph: None,
                        subtree: None,
                        at_epoch: None,
                    },
                )
            })
            .expect("submit")
            .recv()
            .expect("receipt")
            .expect("snapshot reads")
        {
            ReadReply::Snapshot(view) => view,
            other => panic!("{other:?}"),
        };
        views += 1;
        let nodes: BTreeSet<u32> = view.nodes.iter().map(|n| n.node).collect();
        assert_eq!(nodes, expected_nodes, "exactly the committed node set");
        for node in &view.nodes {
            // The project carries no round writes — only the tasks do.
            let Some(Value::Str(title)) = scalar(&view, node.node, title_id) else {
                continue;
            };
            let amount = scalar(&view, node.node, amount_id);
            let Some(Value::I64(amount)) = amount else {
                continue; // round 0's title landed, its amount not yet
            };
            let text = title.to_string();
            let Some(round) = text.strip_prefix('r').and_then(|r| r.parse::<i64>().ok()) else {
                continue; // the creation title, before round 0
            };
            assert!(
                *amount == round + 1 || *amount == round,
                "torn mix on node {}: title {text} with amount {amount}",
                node.node
            );
        }
        assert!(
            view.epoch >= last_marker,
            "the epoch marker never decreases"
        );
        last_marker = view.epoch;
        markers.push(view.epoch);
    }
    for w in writers {
        w.join().expect("writer thread");
    }
    assert!(
        views > 5,
        "the reader actually ran alongside the writers ({views} views)"
    );

    let mut kernel = shutdown_shared(handle).expect("sequencer joins");
    let final_lsn = kernel.last_lsn();
    assert!(
        markers.iter().all(|m| *m <= final_lsn),
        "markers never run past the journal"
    );
    let last = snapshot(&mut kernel);
    assert_eq!(last.epoch, final_lsn);
    for node in &last.nodes {
        if let Some(Value::Str(title)) = scalar(&last, node.node, title_id)
            && title.starts_with('r')
        {
            assert_eq!(title.as_ref(), "r39", "every task finished its rounds");
            assert_eq!(scalar(&last, node.node, amount_id), Some(&Value::I64(40)));
        }
    }
    kernel.close().expect("close");
}

// -- WPT-8.2 --------------------------------------------------------------------

fn snapshot_at(kernel: &mut Kernel, epoch: u64) -> GraphView {
    match read(
        kernel,
        &Read::Snapshot {
            graph: None,
            subtree: None,
            at_epoch: Some(epoch),
        },
    ) {
        ReadReply::Snapshot(view) => view,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

/// WPT-8.2: writes, then snapshots at older epochs, return the exact older
/// views; the live state is unchanged by a time-travel read. Bounds are
/// explicit refusals.
#[test]
fn wpt_8_2_time_travel_returns_the_exact_older_view_and_leaves_live_state_alone() {
    let (dir, mut kernel, project, mut tasks) = populated("wpt_8_2", 2);
    set(&mut kernel, tasks[0], "amount", Value::I64(1));
    let view_a = snapshot(&mut kernel);
    let epoch_a = view_a.epoch;

    log_rows(&mut kernel, tasks[1], &[5, 6, 7]);
    set(&mut kernel, tasks[0], "amount", Value::I64(2));
    set(
        &mut kernel,
        tasks[0],
        "title",
        Value::str("renamed").unwrap(),
    );
    let third = create(&mut kernel, "task", Some(project.0), vec![]);
    tasks.push(third);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: tasks[1],
                dst: tasks[0],
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    let view_b = snapshot(&mut kernel);
    let epoch_b = view_b.epoch;
    assert!(epoch_b > epoch_a);
    assert_eq!(view_b.nodes.len(), view_a.nodes.len() + 1);

    let live = digest(kernel.state());
    // The exact older views, on request.
    assert_eq!(
        snapshot_at(&mut kernel, epoch_a),
        view_a,
        "the view at epoch A is exactly what a live read saw then"
    );
    assert_eq!(snapshot_at(&mut kernel, epoch_b), view_b);
    // An LSN inside a txn's frames resolves to the last COMMIT at or below it.
    let mid = snapshot_at(&mut kernel, epoch_a + 1);
    assert_eq!(
        mid.epoch, epoch_a,
        "the marker is the commit boundary reached"
    );
    assert_eq!(mid.nodes, view_a.nodes);
    assert_eq!(mid.scalars, view_a.scalars);
    assert_eq!(mid.edges, view_a.edges);
    assert_eq!(
        digest(kernel.state()),
        live,
        "time travel never touches the live state"
    );

    // Bounds.
    let err = kernel
        .read(
            0,
            &Read::Snapshot {
                graph: None,
                subtree: None,
                at_epoch: Some(epoch_b + 1),
            },
        )
        .expect_err("beyond the current epoch");
    assert_eq!(err.code, codes::INVALID_KEY);
    assert!(err.hint.contains("at_epoch must be"), "{err}");

    // A checkpoint retires the older snapshot: epochs before it are refused
    // explicitly, later ones still read.
    kernel.checkpoint().expect("checkpoint");
    let retained = kernel.manifest().created_lsn;
    assert!(retained >= epoch_b, "the checkpoint is at or after epoch B");
    let err = kernel
        .read(
            0,
            &Read::Snapshot {
                graph: None,
                subtree: None,
                at_epoch: Some(epoch_a),
            },
        )
        .expect_err("predates the retained snapshot");
    assert_eq!(err.code, codes::NOT_FOUND);
    assert!(err.hint.contains("oldest readable epoch"), "{err}");
    set(&mut kernel, tasks[0], "amount", Value::I64(3));
    let view_c = snapshot(&mut kernel);
    assert_eq!(
        snapshot_at(&mut kernel, retained).nodes.len(),
        view_b.nodes.len()
    );
    assert_eq!(snapshot_at(&mut kernel, view_c.epoch), view_c);

    kernel.close().expect("close");
    let (mut reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(
        snapshot_at(&mut reopened, view_c.epoch),
        view_c,
        "time travel survives a restart"
    );
}

// -- WPT-8.3 --------------------------------------------------------------------

fn expected_log(task_index: usize, k: u64) -> i64 {
    task_index as i64 * 10_000_000 + k as i64
}

/// WPT-8.3: 200 nodes × 5 000 cells = 1 000 000 cells in one column, read
/// through a cursor in row- and byte-budgeted pages: everything comes back
/// in `(node, k)` order, once, with one schema across all batches.
#[test]
fn wpt_8_3_cursor_pages_a_million_cells_in_order_with_one_schema() {
    const NODES: usize = 200;
    const PER_NODE: u64 = 5_000;
    let dir = test_dir("wpt_8_3");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let mut tasks = Vec::new();
    for _ in 0..NODES {
        tasks.push(create(&mut kernel, "task", Some(project.0), vec![]));
    }
    for (i, task) in tasks.iter().enumerate() {
        for block in 0..5u64 {
            let values: Vec<i64> = (block * 1_000..(block + 1) * 1_000)
                .map(|k| expected_log(i, k))
                .collect();
            log_rows(&mut kernel, *task, &values);
        }
    }
    kernel
        .checkpoint()
        .expect("checkpoint — the column now lives in a part");
    // Task ids ascend with creation, so generation order IS (node, k) order.
    let want: Vec<(u32, u64, i64)> = tasks
        .iter()
        .enumerate()
        .flat_map(|(i, t)| (0..PER_NODE).map(move |k| (t.0, k, expected_log(i, k))))
        .collect();
    assert!(want.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)));

    let big = Budget {
        rows: 100_000,
        bytes: ANY_BYTES,
    };
    let cursor = match read(
        &mut kernel,
        &Read::CursorOpen {
            attr: key("log"),
            selector: Selector::All,
            filter: CursorFilter::default(),
        },
    ) {
        ReadReply::Cursor(id) => id,
        other => panic!("{other:?}"),
    };
    let mut got: Vec<(u32, u64, i64)> = Vec::with_capacity(want.len());
    let mut fingerprints = BTreeSet::new();
    let mut batches = 0usize;
    loop {
        match read(
            &mut kernel,
            &Read::CursorFetch {
                cursor,
                budget: big,
            },
        ) {
            ReadReply::Batch(batch) => {
                batches += 1;
                fingerprints.insert(batch.schema.fingerprint());
                assert!(batch.rows.len() <= big.rows);
                assert_eq!(batch.schema.attr, key("log"));
                assert!(batch.schema.vector);
                for row in &batch.rows {
                    assert!(row.lsn > 0, "every cell carries the LSN that wrote it");
                    let Value::I64(v) = row.value else {
                        panic!("i64 column")
                    };
                    got.push((row.node, row.k, v));
                }
                if batch.remaining {
                    assert_eq!(
                        batch.rows.len(),
                        big.rows,
                        "a full page while more is waiting"
                    );
                } else {
                    break;
                }
            }
            other => panic!("expected a batch, got {other:?}"),
        }
    }
    assert_eq!(got.len(), 1_000_000, "no loss, no duplicate");
    assert_eq!(got, want, "everything, in (node, k) order");
    assert_eq!(batches, 10);
    assert_eq!(fingerprints.len(), 1, "one schema across all batches");
    // The cursor is exhausted: further fetches are empty and stable.
    match read(
        &mut kernel,
        &Read::CursorFetch {
            cursor,
            budget: big,
        },
    ) {
        ReadReply::Batch(batch) => {
            assert!(batch.rows.is_empty() && !batch.remaining);
        }
        other => panic!("{other:?}"),
    }
    read(&mut kernel, &Read::CursorClose { cursor });

    // The same column under a byte budget: pages are smaller, content equal.
    let (schemas, rows) = drain(
        &mut kernel,
        "log",
        Selector::All,
        CursorFilter::default(),
        Budget {
            rows: 1_000_000,
            bytes: 64 * 1024,
        },
    );
    assert!(
        schemas.len() > batches,
        "a byte budget splits the column finer"
    );
    assert_eq!(schemas.iter().collect::<BTreeSet<_>>().len(), 1);
    assert_eq!(rows.len(), 1_000_000);
    assert!(
        rows.windows(2)
            .all(|w| (w[0].node, w[0].k) < (w[1].node, w[1].k))
    );
    kernel.close().expect("close");
}

// -- WPT-8.4 --------------------------------------------------------------------

fn blocker_kinds(kernel: &mut Kernel, node: NodeId) -> Vec<(String, String)> {
    match read(kernel, &Read::Blockers { node }) {
        ReadReply::Blockers(list) => list
            .into_iter()
            .map(|b| (b.kind.to_string(), b.detail))
            .collect(),
        other => panic!("expected blockers, got {other:?}"),
    }
}

/// WPT-8.4: a node held back by a pending GUARD, an active INHIBIT and unmet
/// requirement rules. BLOCKERS names all three precisely; once they are
/// removed the list is empty.
#[test]
fn wpt_8_4_blockers_name_guard_inhibition_and_unmet_rule_then_empty() {
    let (_dir, mut kernel, project, tasks) = populated("wpt_8_4", 4);
    let [target, guard_source, inhibitor, ready] = [tasks[0], tasks[1], tasks[2], tasks[3]];
    for node in [target, guard_source, inhibitor] {
        kernel
            .execute(
                0,
                &subject(),
                &Command::Action {
                    node,
                    transition: key("start"),
                    actor: ActorClass::Human,
                    payload: None,
                },
            )
            .expect("start: all three sit in `running`");
    }
    // GUARD from guard_source: sigma `failed` (not reached) → pending.
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: guard_source,
                dst: target,
                kind: kinds::GUARD,
                aux: 0,
                guard: Some((key("failed"), key("auto_done"))),
            },
        )
        .expect("guard edge");
    // INHIBIT from inhibitor: the source is not success-terminal → active.
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: inhibitor,
                dst: target,
                kind: kinds::INHIBIT,
                aux: 0,
                guard: None,
            },
        )
        .expect("inhibit edge");

    let blockers = blocker_kinds(&mut kernel, target);
    let kinds_list: Vec<&str> = blockers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(kinds_list.contains(&"unmet_rule"), "{blockers:?}");
    assert!(kinds_list.contains(&"pending_guard"), "{blockers:?}");
    assert!(kinds_list.contains(&"inhibited"), "{blockers:?}");
    // Precisely named.
    assert!(
        blockers.iter().any(|(k, d)| k == "pending_guard"
            && d.contains("auto_done")
            && d.contains("failed")
            && d.contains(&format!("source {}", guard_source.0))),
        "the guard names its transition, its sigma and its source: {blockers:?}"
    );
    assert!(
        blockers
            .iter()
            .any(|(k, d)| k == "inhibited" && d.contains(&format!("by source {}", inhibitor.0))),
        "the inhibition names its source: {blockers:?}"
    );
    assert!(
        blockers
            .iter()
            .any(|(k, d)| k == "unmet_rule" && d.contains("amount_gte_10")),
        "the unmet rule is named: {blockers:?}"
    );
    // Not blocked: the fourth task never started — it sits in `open` with
    // its title, and nothing holds it back.
    let ready_blockers = blocker_kinds(&mut kernel, ready);
    assert!(
        ready_blockers.is_empty(),
        "a node with a ready action has no blockers: {ready_blockers:?}"
    );

    // Remove them one at a time and watch the entries leave.
    set(&mut kernel, target, "amount", Value::I64(20));
    set(&mut kernel, target, "owner", Value::NodeRef(project));
    let after_rules = blocker_kinds(&mut kernel, target);
    assert!(
        !after_rules.iter().any(|(k, _)| k == "unmet_rule"),
        "{after_rules:?}"
    );
    assert!(after_rules.iter().any(|(k, _)| k == "pending_guard"));
    assert!(after_rules.iter().any(|(k, _)| k == "inhibited"));

    let edge_from = |kernel: &Kernel, src: NodeId| -> u64 {
        let arrays = kernel.state().arrays();
        (0..arrays.edge_slots() as u64)
            .find(|i| {
                arrays
                    .edge_slot(lattice_core::ids::EdgeId(*i as u32))
                    .is_some_and(|r| r.src == src && r.dst == target)
            })
            .expect("the edge is in the table")
    };
    let guard_edge = edge_from(&kernel, guard_source);
    let inhibit_edge = edge_from(&kernel, inhibitor);
    kernel
        .execute(0, &subject(), &Command::Unlink { edge: guard_edge })
        .expect("unlink the guard");
    let after_guard = blocker_kinds(&mut kernel, target);
    assert!(
        after_guard.iter().all(|(k, _)| k != "pending_guard"),
        "{after_guard:?}"
    );
    assert!(
        after_guard.iter().any(|(k, _)| k == "inhibited"),
        "the inhibition is still named"
    );

    kernel
        .execute(0, &subject(), &Command::Unlink { edge: inhibit_edge })
        .expect("unlink the inhibitor");
    assert!(
        blocker_kinds(&mut kernel, target).is_empty(),
        "nothing holds the node back any more"
    );
    // And the action that was blocked now fires.
    kernel
        .execute(
            0,
            &subject(),
            &Command::Action {
                node: target,
                transition: key("close"),
                actor: ActorClass::Human,
                payload: None,
            },
        )
        .expect("close fires once unblocked");
    assert_eq!(
        kernel.state().machine().state_key_of(target).unwrap(),
        key("done")
    );
}

// -- SGT-6.2 + zone pushdown ----------------------------------------------------

/// SGT-6.2: the latest cell of one node in a column of three 10 000-cell
/// nodes is read by decoding exactly ONE run — the read does not scale with
/// the column — and it is the right cell.
#[test]
fn sgt_6_2_latest_cell_read_touches_one_run() {
    let (_dir, mut kernel, project, tasks) = populated("sgt_6_2", 3);
    for (i, task) in tasks.iter().enumerate() {
        for block in 0..10u64 {
            let values: Vec<i64> = (block * 1_000..(block + 1) * 1_000)
                .map(|k| (i as i64 + 1) * 100_000 + k as i64)
                .collect();
            log_rows(&mut kernel, *task, &values);
        }
    }
    let _ = project;
    kernel.checkpoint().expect("checkpoint");
    let node = tasks[1];

    let before = kernel.parts_decoded_runs();
    let (_, rows) = drain(
        &mut kernel,
        "log",
        Selector::Subtree(node),
        CursorFilter {
            k: Some((9_999, 9_999)),
            value: None,
        },
        Budget {
            rows: 1,
            bytes: ANY_BYTES,
        },
    );
    let decoded = kernel.parts_decoded_runs() - before;
    assert_eq!(decoded, 1, "one run decoded for a 30 000-cell column");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].node, node.0);
    assert_eq!(rows[0].k, 9_999);
    assert_eq!(rows[0].value, Value::I64(200_000 + 9_999));
}

/// SPEC-03 §4 zone pushdown through the kernel: a value range that only one
/// run admits decodes exactly that run; a key range no run covers decodes
/// nothing at all.
#[test]
fn zone_maps_and_run_directory_skip_runs_without_decoding() {
    let (_dir, mut kernel, _project, tasks) = populated("pushdown", 50);
    for (i, task) in tasks.iter().enumerate() {
        let values: Vec<i64> = (0..100i64).map(|j| i as i64 * 1_000 + j).collect();
        log_rows(&mut kernel, *task, &values);
    }
    kernel.checkpoint().expect("checkpoint");
    assert!(
        kernel
            .part_columns()
            .iter()
            .any(|((_, a), _)| *a == attr_id(&kernel, "log"))
    );

    let before = kernel.parts_decoded_runs();
    let (_, rows) = drain(
        &mut kernel,
        "log",
        Selector::All,
        CursorFilter {
            k: None,
            value: Some((Value::I64(5_000), Value::I64(5_050))),
        },
        Budget {
            rows: 10,
            bytes: ANY_BYTES,
        },
    );
    assert_eq!(
        kernel.parts_decoded_runs() - before,
        1,
        "only the admitting run was decoded"
    );
    assert_eq!(rows.len(), 51);
    assert!(rows.iter().all(|r| r.node == tasks[5].0));
    assert_eq!(rows.first().unwrap().k, 0);
    assert_eq!(rows.last().unwrap().k, 50);

    let before = kernel.parts_decoded_runs();
    let (schemas, rows) = drain(
        &mut kernel,
        "log",
        Selector::All,
        CursorFilter {
            k: Some((5_000, 6_000)),
            value: None,
        },
        Budget {
            rows: 10,
            bytes: ANY_BYTES,
        },
    );
    assert_eq!(
        kernel.parts_decoded_runs() - before,
        0,
        "no run covers that key range"
    );
    assert!(rows.is_empty());
    assert_eq!(
        schemas.len(),
        1,
        "an exhausted cursor still names its schema"
    );

    // The engine sees the same cells the cursor returned — pushdown is an
    // optimisation of the read, never a different answer.
    let all = cursor_cells(&mut kernel, "log", 7);
    assert_eq!(all.len(), 5_000);
    assert_eq!(
        all.iter()
            .map(|r| (r.node, r.k, format!("{:?}", r.value)))
            .collect::<BTreeSet<_>>(),
        engine_cells(&kernel, "log")
            .iter()
            .map(|(n, k, _, v)| (*n, *k, v.clone()))
            .collect::<BTreeSet<_>>()
    );
}

// -- node read, history, cursors ------------------------------------------------

#[test]
fn node_read_returns_scalars_records_and_vector_previews() {
    let (_dir, mut kernel, project, tasks) = populated("node_read", 2);
    let task = tasks[0];
    set(&mut kernel, task, "amount", Value::I64(42));
    set(&mut kernel, task, "prio", Value::Enum(1));
    set(
        &mut kernel,
        task,
        "evidence",
        Value::Record(RecordValue::from_pairs(&[(1, Value::str("note").unwrap())]).unwrap()),
    );
    log_rows(&mut kernel, task, &[10, 11, 12, 13, 14, 15]);
    // `options()` previews 4 cells at each end.
    let node = match read(&mut kernel, &Read::NodeRead { node: task }) {
        ReadReply::Node(view) => view,
        other => panic!("{other:?}"),
    };
    assert_eq!(node.node.node, task.0);
    assert_eq!(node.node.parent, Some(project.0));
    assert_eq!(node.node.type_key.as_ref(), Some(&key("task")));
    assert_eq!(node.node.state_key.as_ref(), Some(&key("open")));
    assert_eq!(node.scalars.len(), 4, "title, amount, prio, evidence");
    let scalar_of = |name: &str| {
        node.scalars
            .iter()
            .find(|(k, _)| *k == key(name))
            .map(|(_, v)| v.clone())
            .expect("scalar present")
    };
    assert_eq!(scalar_of("amount"), Value::I64(42));
    assert_eq!(scalar_of("prio"), Value::Enum(1));
    assert_eq!(scalar_of("title"), Value::str("task 0").unwrap());
    // Scalars come back attribute-ascending.
    let names: Vec<&str> = node.scalars.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, vec!["amount", "evidence", "prio", "title"]);
    assert_eq!(node.vectors.len(), 1, "only attributes with cells appear");
    let log = &node.vectors[0];
    assert_eq!(log.attr, key("log"));
    assert_eq!(log.count, 6);
    assert_eq!((log.first_k, log.last_k), (Some(0), Some(5)));
    assert_eq!(
        log.head.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        log.tail.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
        vec![
            Value::I64(12),
            Value::I64(13),
            Value::I64(14),
            Value::I64(15)
        ]
    );

    let err = kernel
        .read(
            0,
            &Read::NodeRead {
                node: NodeId(9_999),
            },
        )
        .expect_err("no such node");
    assert_eq!(err.code, codes::NOT_FOUND);
}

#[test]
fn history_lists_the_nodes_events_in_lsn_order_with_authors() {
    let (_dir, mut kernel, project, tasks) = populated("history", 3);
    let [task, other] = [tasks[0], tasks[1]];
    set(&mut kernel, task, "title", Value::str("ship it").unwrap());
    log_rows(&mut kernel, task, &[1, 2]);
    log_rows(&mut kernel, other, &[9]);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: task,
                dst: other,
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    kernel
        .execute(
            0,
            &subject(),
            &Command::Action {
                node: task,
                transition: key("spawn_sub"),
                actor: ActorClass::Agent,
                payload: Some(Value::str("go").unwrap()),
            },
        )
        .expect_err("spawn_sub is human-only");
    kernel
        .execute(
            0,
            &subject(),
            &Command::Action {
                node: task,
                transition: key("spawn_sub"),
                actor: ActorClass::Human,
                payload: Some(Value::str("go").unwrap()),
            },
        )
        .expect("spawn");
    let child = kernel.state().arrays().children_of(task)[0];
    kernel
        .execute(0, &subject(), &Command::Unlink { edge: 0 })
        .expect("unlink");

    let events = match read(
        &mut kernel,
        &Read::History {
            node: task,
            range: None,
        },
    ) {
        ReadReply::History(events) => events,
        other => panic!("{other:?}"),
    };
    let kinds: Vec<&str> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        // Creation is a vertex + its initial title; the refused agent
        // action left no record.
        vec![
            "node_added",
            "attr_set",
            "attr_set",
            "rows_appended",
            "edge_added",
            "action",
            "edge_removed",
        ],
        "every record naming the node, in journal order, and nothing else"
    );
    assert!(
        events.windows(2).all(|w| w[0].lsn < w[1].lsn),
        "ascending by LSN"
    );
    assert!(
        events[0]
            .detail
            .contains(&format!("under node {}", project.0)),
        "{}",
        events[0].detail
    );
    assert_eq!(
        events[5].author.as_deref(),
        Some("test-subject"),
        "actions carry their author"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.kind != "action")
            .all(|e| e.author.is_none())
    );
    assert!(
        events[4].detail.contains("to node")
            && events[4].detail.contains(&format!("node {}", other.0))
    );
    assert!(events[5].detail.contains("spawn_sub") && events[5].detail.contains("open → running"));
    assert_eq!(
        events[5].detail.matches("Agent").count(),
        0,
        "the refused agent action left no record"
    );

    // The spawned child's history is its creation; a range narrows by LSN.
    let child_events = match read(
        &mut kernel,
        &Read::History {
            node: child,
            range: None,
        },
    ) {
        ReadReply::History(events) => events,
        other => panic!("{other:?}"),
    };
    assert_eq!(child_events.len(), 1);
    assert_eq!(child_events[0].kind, "node_added");
    assert!(child_events[0].detail.contains("spawned"));
    let (from, to) = (events[2].lsn, events[3].lsn);
    let ranged = match read(
        &mut kernel,
        &Read::History {
            node: task,
            range: Some((from, to)),
        },
    ) {
        ReadReply::History(events) => events,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        ranged.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec!["attr_set", "rows_appended"]
    );
}

#[test]
fn cursors_belong_to_their_session_and_expire_after_their_idle_ttl() {
    let dir = test_dir("cursor_ttl");
    let clock = test_clock();
    let ttl_us = 60_000_000u64;
    let mut kernel =
        Kernel::create(&dir, 1, options_with(Arc::clone(&clock))).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("x").unwrap())],
    );
    log_rows(&mut kernel, task, &[1, 2, 3]);

    // Cursors are session-scoped: a cursor opened under session 7 is
    // invisible to session 0.
    let mine = match kernel
        .read(
            7,
            &Read::CursorOpen {
                attr: key("log"),
                selector: Selector::All,
                filter: CursorFilter::default(),
            },
        )
        .expect("open")
    {
        ReadReply::Cursor(id) => id,
        other => panic!("{other:?}"),
    };
    let err = kernel
        .read(
            0,
            &Read::CursorFetch {
                cursor: mine,
                budget: Budget {
                    rows: 10,
                    bytes: ANY_BYTES,
                },
            },
        )
        .expect_err("other session");
    assert_eq!(err.code, codes::NOT_FOUND);
    assert!(
        kernel.open_cursors() == vec![mine],
        "a refused foreign fetch does not drop the cursor"
    );

    let any = Budget {
        rows: 10,
        bytes: ANY_BYTES,
    };
    // Within the ttl a fetch works and refreshes the idle timer.
    clock.fetch_add(ttl_us, std::sync::atomic::Ordering::Relaxed);
    kernel
        .read(
            7,
            &Read::CursorFetch {
                cursor: mine,
                budget: any,
            },
        )
        .expect("still alive at exactly the ttl");
    clock.fetch_add(ttl_us, std::sync::atomic::Ordering::Relaxed);
    kernel
        .read(
            7,
            &Read::CursorFetch {
                cursor: mine,
                budget: any,
            },
        )
        .expect("the fetch refreshed the timer");

    // Idle past the ttl: gone, with a telling hint.
    clock.fetch_add(ttl_us + 1, std::sync::atomic::Ordering::Relaxed);
    let err = kernel
        .read(
            7,
            &Read::CursorFetch {
                cursor: mine,
                budget: any,
            },
        )
        .expect_err("expired");
    assert_eq!(err.code, codes::NOT_FOUND, "no code outside the registry");
    assert!(err.detail.contains("expired"), "{err}");
    assert!(
        kernel.open_cursors().is_empty(),
        "an expired cursor is reclaimed"
    );

    let again = match read(
        &mut kernel,
        &Read::CursorOpen {
            attr: key("log"),
            selector: Selector::All,
            filter: CursorFilter::default(),
        },
    ) {
        ReadReply::Cursor(id) => id,
        other => panic!("{other:?}"),
    };
    // A budget without rows is a malformed request, not an empty page.
    let err = kernel
        .read(
            0,
            &Read::CursorFetch {
                cursor: again,
                budget: Budget {
                    rows: 0,
                    bytes: ANY_BYTES,
                },
            },
        )
        .expect_err("zero rows");
    assert_eq!(err.code, codes::INVALID_KEY);
    read(&mut kernel, &Read::CursorClose { cursor: again });
    let err = kernel
        .read(
            0,
            &Read::CursorFetch {
                cursor: again,
                budget: any,
            },
        )
        .expect_err("closed");
    assert!(err.hint.contains("open cursors: []"), "{err}");
}

#[test]
fn selectors_and_filters_refuse_what_they_cannot_mean() {
    let (_dir, mut kernel, _project, tasks) = populated("selectors", 4);
    let mut by_two = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        log_rows(&mut kernel, *task, &[i as i64]);
        if i % 2 == 0 {
            set(&mut kernel, *task, "title", Value::str("even").unwrap());
            by_two.push(task.0);
        }
    }
    let select = |kernel: &mut Kernel, selector: Selector| -> BTreeSet<u32> {
        cursor_cells_with(kernel, "log", selector)
            .iter()
            .map(|r| r.node)
            .collect()
    };
    assert_eq!(
        select(&mut kernel, Selector::All),
        tasks.iter().map(|t| t.0).collect()
    );
    assert_eq!(
        select(
            &mut kernel,
            Selector::TypeScan {
                node_type: key("task")
            }
        ),
        tasks.iter().map(|t| t.0).collect()
    );
    assert!(
        select(
            &mut kernel,
            Selector::TypeScan {
                node_type: key("project")
            }
        )
        .is_empty(),
        "projects have no log cells"
    );
    assert_eq!(
        select(
            &mut kernel,
            Selector::ByState {
                node_type: key("task"),
                state: key("open")
            }
        ),
        tasks.iter().map(|t| t.0).collect()
    );
    assert!(
        select(
            &mut kernel,
            Selector::ByState {
                node_type: key("task"),
                state: key("running")
            }
        )
        .is_empty()
    );
    assert_eq!(
        select(&mut kernel, Selector::Graph(key("pm"))),
        tasks.iter().map(|t| t.0).collect()
    );
    // Every task carries a title — creation wrote one.
    assert_eq!(
        select(&mut kernel, Selector::AttrPresence { attr: key("title") }),
        tasks.iter().map(|t| t.0).collect()
    );
    assert_eq!(
        select(
            &mut kernel,
            Selector::SharedKey {
                attr: key("title"),
                value: Value::str("even").unwrap()
            }
        ),
        by_two.iter().copied().collect()
    );
    let root = tasks[0];
    let subtask = create(&mut kernel, "subtask", Some(root.0), vec![]);
    log_rows(&mut kernel, subtask, &[1]);
    let mut subtree: BTreeSet<u32> = tasks.iter().map(|t| t.0).filter(|n| *n == root.0).collect();
    subtree.insert(subtask.0);
    assert_eq!(select(&mut kernel, Selector::Subtree(root)), subtree);

    // Unknown names and malformed filters are refusals with the hint law.
    let open = |kernel: &mut Kernel, attr: &str, selector: Selector, filter: CursorFilter| {
        kernel.read(
            0,
            &Read::CursorOpen {
                attr: key(attr),
                selector,
                filter,
            },
        )
    };
    let err = open(
        &mut kernel,
        "log",
        Selector::TypeScan {
            node_type: key("nope"),
        },
        CursorFilter::default(),
    )
    .expect_err("unknown type");
    assert_eq!(err.code, codes::NOT_FOUND);
    assert!(err.hint.contains("known types:"), "{err}");
    let err = open(
        &mut kernel,
        "log",
        Selector::ByState {
            node_type: key("task"),
            state: key("late"),
        },
        CursorFilter::default(),
    )
    .expect_err("unknown state");
    assert!(err.hint.contains("known states:"), "{err}");
    let err = open(
        &mut kernel,
        "colour",
        Selector::All,
        CursorFilter::default(),
    )
    .expect_err("unknown attribute");
    assert_eq!(err.code, codes::SCHEMA);
    assert!(err.hint.contains("known attributes:"), "{err}");
    let err = open(
        &mut kernel,
        "log",
        Selector::All,
        CursorFilter {
            k: Some((5, 2)),
            value: None,
        },
    )
    .expect_err("empty k range");
    assert_eq!(err.code, codes::INVALID_KEY);
    let err = open(
        &mut kernel,
        "log",
        Selector::All,
        CursorFilter {
            k: None,
            value: Some((Value::str("a").unwrap(), Value::str("z").unwrap())),
        },
    )
    .expect_err("string bounds on an i64 column");
    assert_eq!(err.code, codes::SCHEMA);
    let err = kernel
        .read(
            0,
            &Read::Snapshot {
                graph: Some(key("no_graph")),
                subtree: None,
                at_epoch: None,
            },
        )
        .expect_err("unknown graph");
    assert!(err.hint.contains("known graphs: pm"), "{err}");
    let err = kernel
        .read(
            0,
            &Read::Snapshot {
                graph: None,
                subtree: Some(NodeId(9_999)),
                at_epoch: None,
            },
        )
        .expect_err("unknown root");
    assert_eq!(err.code, codes::NOT_FOUND);
}

fn cursor_cells_with(kernel: &mut Kernel, attr: &str, selector: Selector) -> Vec<CellRow> {
    drain(
        kernel,
        attr,
        selector,
        CursorFilter::default(),
        Budget {
            rows: 100,
            bytes: ANY_BYTES,
        },
    )
    .1
}

#[test]
fn a_snapshot_scopes_to_a_graph_or_subtree_and_reads_your_own_txn() {
    let (_dir, mut kernel, project, tasks) = populated("scopes", 3);
    let child = create(
        &mut kernel,
        "subtask",
        Some(tasks[0].0),
        vec![("title", Value::str("c").unwrap())],
    );
    let whole = snapshot(&mut kernel);
    assert_eq!(whole.nodes.len(), 5, "project + 3 tasks + child");
    assert_eq!(whole.edges.len(), 0);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: tasks[1],
                dst: tasks[2],
                kind: kinds::OR,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: child,
                dst: tasks[1],
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    let by_graph = match read(
        &mut kernel,
        &Read::Snapshot {
            graph: Some(key("pm")),
            subtree: None,
            at_epoch: None,
        },
    ) {
        ReadReply::Snapshot(view) => view,
        other => panic!("{other:?}"),
    };
    assert_eq!(by_graph.nodes.len(), 5);
    assert_eq!(
        by_graph.edges.len(),
        2,
        "edges are listed when their source is in scope"
    );
    // Edge ids are table slots, in order.
    assert_eq!(
        by_graph.edges.iter().map(|e| e.edge_id).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(by_graph.edges[0].src, tasks[1].0);
    assert_eq!(by_graph.edges[1].src, child.0);
    let subtree = match read(
        &mut kernel,
        &Read::Snapshot {
            graph: None,
            subtree: Some(tasks[0]),
            at_epoch: None,
        },
    ) {
        ReadReply::Snapshot(view) => view,
        other => panic!("{other:?}"),
    };
    let in_subtree: BTreeSet<u32> = subtree.nodes.iter().map(|n| n.node).collect();
    assert_eq!(in_subtree, BTreeSet::from([tasks[0].0, child.0]));
    assert_eq!(
        subtree.edges.len(),
        1,
        "only the child's outgoing edge has its source inside"
    );
    assert_eq!(
        subtree.scalars.len(),
        2,
        "the titles of the task and its child"
    );

    // Read-your-writes: the session's scratch is what IT sees; others see
    // the live state.
    kernel.begin_txn(5).expect("begin");
    kernel
        .execute(
            5,
            &subject(),
            &Command::AttrSet {
                node: tasks[0],
                attr: key("amount"),
                value: Value::I64(77),
            },
        )
        .expect("in txn");
    let mine = match kernel
        .read(
            5,
            &Read::Snapshot {
                graph: None,
                subtree: Some(tasks[0]),
                at_epoch: None,
            },
        )
        .expect("read")
    {
        ReadReply::Snapshot(view) => view,
        other => panic!("{other:?}"),
    };
    let others = match kernel
        .read(
            9,
            &Read::Snapshot {
                graph: None,
                subtree: Some(tasks[0]),
                at_epoch: None,
            },
        )
        .expect("read")
    {
        ReadReply::Snapshot(view) => view,
        other => panic!("{other:?}"),
    };
    assert!(
        mine.scalars
            .iter()
            .any(|c| c.attr.0 == attr_id(&kernel, "amount"))
    );
    assert!(
        !others
            .scalars
            .iter()
            .any(|c| c.attr.0 == attr_id(&kernel, "amount"))
    );
    kernel.rollback_txn(5).expect("rollback");
    let _ = project;
}
