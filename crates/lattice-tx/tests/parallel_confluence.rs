//! T-310 acceptance on the DURABLE kernel (ARCHITECTURE §8 — completed by
//! T-313): concurrent subtree-locked writers over the single sequencer.
//!
//! - Confluence: any interleave of parallel writers converges to the
//!   journal's own replay (live state == reopened state).
//! - Per-writer order: a writer holding a node's U-lock across submit→receipt
//!   keeps ITS commands on that node in order.
//! - Ordered acquisition is deadlock-free.
//! - LSNs stay dense and every submitted command is answered (WPT-5.1
//!   parallel pipelining variant).

mod common;

use common::*;
use lattice_core::ids::NodeId;
use lattice_core::value::Value;
use lattice_engine::dispatch::kinds;
use lattice_tx::{
    Command, Kernel, KernelHandle, Reply, Row, Subject, SubtreeLockTable, WriterPool, codes,
    conflict_domain, shutdown_shared,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A database with the fixture schema, `projects` projects and `tasks` tasks
/// spread under them.
fn database(name: &str, projects: usize, tasks: usize) -> (PathBuf, Kernel, Vec<NodeId>) {
    let dir = test_dir(name);
    let mut kernel = Kernel::create(&dir, 1, limits()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let created = |kernel: &mut Kernel, cmd: Command| match kernel.execute(0, &subject(), &cmd) {
        Ok(Reply::Created(id)) => id,
        Ok(other) => panic!("expected Created, got {other:?}"),
        Err(err) => panic!("setup command refused: {err}"),
    };
    let mut roots = Vec::new();
    for _ in 0..projects {
        roots.push(created(
            &mut kernel,
            Command::CreateNode {
                graph: key("pm"),
                node_type: key("project"),
                parent: None,
                attrs: vec![],
            },
        ));
    }
    let mut nodes = Vec::new();
    for i in 0..tasks {
        nodes.push(created(
            &mut kernel,
            Command::CreateNode {
                graph: key("pm"),
                node_type: key("task"),
                parent: Some(roots[i % roots.len()]),
                attrs: vec![(key("title"), Value::str(format!("task {i}")).unwrap())],
            },
        ));
    }
    (dir, kernel, nodes)
}

/// One one-cell row append to the node's `log` column (k is assigned by the
/// kernel, so concurrent appends never collide on a key).
fn row(node: NodeId, value: i64) -> Command {
    Command::RowAppend {
        node,
        set: key("task_table"),
        rows: vec![Row {
            k_skip: 0,
            cells: vec![(key("log"), Value::I64(value))],
        }],
    }
}

/// The node's `log` values in k order.
fn log_cells(kernel: &Kernel, node: NodeId) -> Vec<i64> {
    let machine = kernel.state().machine();
    let log = machine.compiled_ref().attr_ids[&key("log")];
    let mut cells: Vec<(u64, i64)> = machine
        .cells()
        .filter(|((n, a, _), _)| *n == node && *a == log)
        .filter_map(|((_, _, k), cell)| match cell.value {
            Value::I64(v) => Some((k.0, v)),
            _ => None,
        })
        .collect();
    cells.sort_by_key(|(k, _)| *k);
    cells.into_iter().map(|(_, v)| v).collect()
}

/// Committed txns on disk (the journal's own count; `recover` refuses gaps).
fn journal_txn_count(dir: &Path) -> usize {
    let mut count = 0usize;
    lattice_storage::journal::recover(&dir.join("journal"), |_| count += 1)
        .expect("journal recovers — LSNs are dense");
    count
}

/// Confluence: 8 writers, interleaved row appends and title writes across 24
/// tasks. Whatever the interleave, the live state equals the journal's own
/// replay, every command is answered, and no append is lost or doubled.
#[test]
fn confluence_under_parallel_load_converges_to_journal_replay() {
    const ROUNDS: usize = 8;
    let (dir, kernel, tasks) = database("confluence", 3, 24);
    let setup_txns = journal_txn_count(&dir);
    let handle = Arc::new(KernelHandle::spawn(kernel, 64).expect("sequencer"));
    let pool = WriterPool::spawn(Arc::clone(&handle), 8, 32).expect("pool");
    let mut submitted = 0u64;
    for round in 0..ROUNDS {
        for (i, task) in tasks.iter().enumerate() {
            pool.enqueue(submitted, row(*task, i as i64))
                .expect("enqueue");
            submitted += 1;
            pool.enqueue(
                submitted,
                Command::AttrSet {
                    node: *task,
                    attr: key("title"),
                    value: Value::str(format!("round {round}")).unwrap(),
                },
            )
            .expect("enqueue");
            submitted += 1;
        }
    }
    let outcomes = pool.shutdown().expect("writers join");
    assert_eq!(outcomes.len() as u64, submitted, "every command answered");
    assert!(
        outcomes.iter().all(|(_, r)| r.is_ok()),
        "no refusals under parallel load"
    );
    let kernel = shutdown_shared(handle).expect("sequencer joins");

    for task in &tasks {
        assert_eq!(
            log_cells(&kernel, *task).len(),
            ROUNDS,
            "each append landed exactly once"
        );
        // The title is whichever whole round write the sequencer ordered
        // last — one of the rounds, never a torn or mixed value.
        let title = match &kernel
            .state()
            .machine()
            .latest_cell(*task, &key("title"))
            .expect("title")
            .value
        {
            Value::Str(s) => s.to_string(),
            other => panic!("unexpected title value {other:?}"),
        };
        assert!(
            (0..ROUNDS).any(|r| title == format!("round {r}")),
            "a whole round title: {title}"
        );
    }
    // Dense LSNs: one txn of three frames per command after the setup.
    let txns = journal_txn_count(&dir);
    assert_eq!(txns, setup_txns + submitted as usize);
    let last = kernel.last_lsn();
    let live = digest(kernel.state());
    kernel.close().expect("close");

    let (reopened, report) = Kernel::open(&dir, limits()).expect("reopen");
    assert_eq!(report.txns_replayed, txns);
    assert_eq!(report.last_lsn, last);
    assert_eq!(
        digest(reopened.state()),
        live,
        "any interleave converges to the journal's own replay"
    );
}

/// WPT-5.1 parallel variant: 1000 pipelined writes through the pool; every
/// request is answered and the final state is exactly a serial replay.
#[test]
fn wpt_5_1_parallel_pipelining_variant() {
    let (dir, kernel, tasks) = database("wpt_5_1", 2, 20);
    let setup_txns = journal_txn_count(&dir);
    let handle = Arc::new(KernelHandle::spawn(kernel, 256).expect("sequencer"));
    let pool = WriterPool::spawn(Arc::clone(&handle), 4, 128).expect("pool");
    let mut rows = 0usize;
    for i in 0..1000u64 {
        let task = tasks[(i as usize) % tasks.len()];
        let cmd = if i % 5 == 4 {
            // Titles carry no order constraint, so any interleave is legal.
            Command::AttrSet {
                node: task,
                attr: key("title"),
                value: Value::str(format!("t{i}")).unwrap(),
            }
        } else {
            rows += 1;
            row(task, i as i64)
        };
        pool.enqueue(i, cmd).expect("enqueue");
    }
    let outcomes = pool.shutdown().expect("writers join");
    assert_eq!(outcomes.len(), 1000, "all request ids answered");
    assert!(outcomes.iter().all(|(_, r)| r.is_ok()), "no refusals");
    let kernel = shutdown_shared(handle).expect("sequencer joins");
    let landed: usize = tasks.iter().map(|t| log_cells(&kernel, *t).len()).sum();
    assert_eq!(landed, rows, "exactly the row appends landed");
    assert_eq!(
        journal_txn_count(&dir),
        setup_txns + 1000,
        "one txn per command"
    );
    let live = digest(kernel.state());
    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, limits()).expect("reopen");
    assert_eq!(
        digest(reopened.state()),
        live,
        "final state equals serial replay"
    );
}

/// Per-writer order: four writers append to ONE node. Each takes the node's
/// U-lock across submit → receipt, so its own values land in its own order;
/// the axis holds every cell exactly once.
#[test]
fn single_node_contention_preserves_each_writers_order() {
    const WRITERS: i64 = 4;
    const PER_WRITER: i64 = 150;
    let (dir, kernel, tasks) = database("contention", 1, 1);
    let task = tasks[0];
    assert_eq!(
        conflict_domain(&row(task, 0)),
        vec![task],
        "the conflict domain is the target node"
    );
    let handle = Arc::new(KernelHandle::spawn(kernel, 8).expect("sequencer"));
    let locks = Arc::new(SubtreeLockTable::new());
    let mut joins = Vec::new();
    for w in 0..WRITERS {
        let handle = Arc::clone(&handle);
        let locks = Arc::clone(&locks);
        joins.push(std::thread::spawn(move || {
            for seq in 0..PER_WRITER {
                let _guard = locks.acquire(vec![task]).expect("lock");
                let rx = handle
                    .submit(move |kernel| {
                        kernel.execute(0, &Subject::trusted("writer"), &row(task, w * 1000 + seq))
                    })
                    .expect("submit");
                rx.recv().expect("receipt").expect("append accepted");
            }
        }));
    }
    for j in joins {
        j.join().expect("writer thread");
    }
    assert_eq!(
        locks.held_count().expect("lock table healthy"),
        0,
        "every lock released"
    );
    let kernel = shutdown_shared(handle).expect("sequencer joins");
    let cells = log_cells(&kernel, task);
    assert_eq!(
        cells.len(),
        (WRITERS * PER_WRITER) as usize,
        "no append lost or doubled"
    );
    for w in 0..WRITERS {
        let mine: Vec<i64> = cells
            .iter()
            .copied()
            .filter(|v| (w * 1000..w * 1000 + PER_WRITER).contains(v))
            .map(|v| v - w * 1000)
            .collect();
        assert_eq!(
            mine,
            (0..PER_WRITER).collect::<Vec<_>>(),
            "writer {w}'s appends kept their order on the node"
        );
    }
    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, limits()).expect("reopen");
    assert_eq!(log_cells(&reopened, task), cells);
}

/// Deadlock freedom: concurrent linkers take two-node domains in both
/// orders. Every command completes (accepted or refused), none hangs.
#[test]
fn ordered_acquisition_is_deadlock_free() {
    let (dir, kernel, tasks) = database("deadlock", 2, 6);
    let handle = Arc::new(KernelHandle::spawn(kernel, 8).expect("sequencer"));
    let locks = Arc::new(SubtreeLockTable::new());
    let answered = Arc::new(AtomicU64::new(0));
    let mut joins = Vec::new();
    for t in 0..8u64 {
        let handle = Arc::clone(&handle);
        let locks = Arc::clone(&locks);
        let answered = Arc::clone(&answered);
        let tasks = tasks.clone();
        joins.push(std::thread::spawn(move || {
            for i in 0..60u64 {
                let a = tasks[((t * 7 + i * 5) % tasks.len() as u64) as usize];
                let b = tasks[((t * 11 + i * 3 + 1) % tasks.len() as u64) as usize];
                let cmd = Command::Link {
                    src: a,
                    dst: b,
                    kind: kinds::AND,
                    aux: 0,
                    guard: None,
                };
                let _guard = locks.acquire(conflict_domain(&cmd)).expect("lock");
                let rx = handle
                    .submit(move |kernel| kernel.execute(0, &Subject::trusted("linker"), &cmd))
                    .expect("submit");
                if let Err(err) = rx.recv().expect("receipt") {
                    // Self-loops and cycles are the only refusals here.
                    assert!(
                        err.code == codes::CYCLE || err.code == codes::INVALID_KIND_PARAMS,
                        "{err}"
                    );
                }
                answered.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for j in joins {
        j.join().expect("linker thread");
    }
    assert_eq!(
        answered.load(Ordering::Relaxed),
        8 * 60,
        "every linker completed"
    );
    assert_eq!(locks.held_count().expect("lock table healthy"), 0);
    shutdown_shared(handle)
        .expect("join")
        .close()
        .expect("close");
    let _ = dir;
}

/// The inline (single-threaded) sequencer: each command is exactly one txn
/// of three frames, and the LSN sequence stays dense.
#[test]
fn inline_sequencer_is_dense_and_exact() {
    let (dir, mut kernel, tasks) = database("inline", 1, 2);
    let lsn0 = kernel.last_lsn();
    for (i, task) in tasks.iter().enumerate() {
        kernel
            .execute(0, &subject(), &row(*task, i as i64))
            .expect("append");
        assert_eq!(kernel.last_lsn(), lsn0 + 3 * (i as u64 + 1));
    }
    kernel.close().expect("close");
    let (reopened, report) = Kernel::open(&dir, limits()).expect("reopen");
    assert_eq!(report.last_lsn, lsn0 + 6);
    assert_eq!(log_cells(&reopened, tasks[0]), vec![0]);
    assert_eq!(log_cells(&reopened, tasks[1]), vec![1]);
}
