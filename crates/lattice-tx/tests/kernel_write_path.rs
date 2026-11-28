//! T-313 acceptance, part 1 — the write path of the assembled kernel
//! (ARCHITECTURE §5.1) over a real database directory:
//!
//! - WPT-5.2 — txn atomicity: a failing command mid-txn leaves nothing applied
//!   and no LSN range behind; the error names the failing command.
//! - WPT-7.1 — row-append batch = one journal record; gaps preserved.
//! - WPT-7.3 — cascade guard: `HAS_DEPENDENTS` without cascade; ordered
//!   `DEL_VERTEX` records with cascade.
//! - WPT-7.4 — action error shape: `REQUIREMENTS_UNMET` names the failing
//!   rule keys; the same command succeeds once they are met.
//! - SGT-9.2 — state motion + the ACTION record in the journal.
//! - SGT-9.5 — spawn effect: the engine-authored `ADD_VERTEX` rides the same
//!   journal txn.
//! - SFT-6.4 — append after a torn tail continues gap-free.
//! - SGT-7.3 shape — state locks refuse exactly the locked attribute set.
//!
//! Every test asserts durable outcome: the journal's content and/or the state
//! after a reopen — never "the call returned Ok".

mod common;

use common::*;
use lattice_core::ids::NodeId;
use lattice_core::value::Value;
use lattice_engine::dispatch::kinds;
use lattice_schema::entities::ActorClass;
use lattice_schema::key::Key;
use lattice_storage::journal::{self, RecordKind};
use lattice_tx::{
    Command, Kernel, KernelError, Record, RecordData, Reply, Row, Subject, TxnLsns, codes,
};
use std::path::Path;

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
    let reply = kernel
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
    assert_eq!(reply, Reply::Attr { node });
}

/// `set`, but as another session (explicit txns are per session).
fn set_as(kernel: &mut Kernel, session: u64, node: NodeId, attr: &str, value: Value) {
    kernel
        .execute(
            session,
            &Subject::trusted("other-session"),
            &Command::AttrSet {
                node,
                attr: key(attr),
                value,
            },
        )
        .expect("attr set");
}

fn action(kernel: &mut Kernel, node: NodeId, transition: &str) -> Result<Reply, KernelError> {
    kernel.execute(
        0,
        &subject(),
        &Command::Action {
            node,
            transition: key(transition),
            actor: ActorClass::Human,
            payload: None,
        },
    )
}

fn delete(kernel: &mut Kernel, node: NodeId, cascade: bool) -> Result<Reply, KernelError> {
    kernel.execute(0, &subject(), &Command::DeleteNode { node, cascade })
}

fn refused<T: std::fmt::Debug>(result: Result<T, KernelError>, code: u16) -> KernelError {
    let err = result.expect_err("command must be refused");
    assert_eq!(err.code, code, "wrong code: {err}");
    err
}

/// Committed txns of a database directory: `(first_lsn, last_lsn, records)`.
fn journal_txns(dir: &Path) -> Vec<(u64, u64, Vec<Record>)> {
    let mut out = Vec::new();
    journal::recover(&dir.join("journal"), |txn| {
        let records = txn
            .records
            .iter()
            .filter(|r| r.kind != RecordKind::Commit)
            .map(|r| Record::decode(r.kind, &r.payload).expect("record decodes"))
            .collect();
        out.push((txn.first_lsn, txn.last_lsn, records));
    })
    .expect("journal recovers");
    out
}

/// Every cell of one attribute on one node: `(k, value)` ascending.
fn cells_of(kernel: &Kernel, node: NodeId, attr: &Key) -> Vec<(u64, Value)> {
    let machine = kernel.state().machine();
    let attr_id = machine.compiled_ref().attr_ids[attr];
    let mut cells: Vec<(u64, Value)> = machine
        .cells()
        .filter(|((n, a, _), _)| *n == node && *a == attr_id)
        .map(|((_, _, k), cell)| (k.0, cell.value.clone()))
        .collect();
    cells.sort_by_key(|(k, _)| *k);
    cells
}

fn state_key(kernel: &Kernel, node: NodeId) -> Key {
    kernel
        .state()
        .machine()
        .state_key_of(node)
        .expect("state key")
}

// -- WPT-5.2 -------------------------------------------------------------------

/// WPT-5.2: TXN of 3 commands, middle invalid → none of the 3 applied, the
/// single LSN range is absent, and the error names the failing command.
#[test]
fn wpt_5_2_txn_atomicity_none_applied_and_error_names_the_failing_command() {
    let dir = test_dir("wpt_5_2");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let live_before = digest(kernel.state());
    let lsn_before = kernel.last_lsn();

    kernel.begin_txn(0).expect("begin");
    let task = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("a").unwrap())],
    );
    // Read-your-writes: the scratch carries command 1 …
    assert!(kernel.read_state(0).arrays().is_alive(task));
    // … the live state does not.
    assert!(!kernel.state().arrays().is_alive(task));

    // Command 2 — the middle one — is invalid; command 3 never runs.
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("pm"),
                node_type: key("no_such_type"),
                parent: Some(project),
                attrs: vec![],
            },
        ),
        codes::NOT_FOUND,
    );
    assert!(
        err.hint
            .contains("known types: project, sealed_task, subtask, task"),
        "the error names the failing command's problem: {err}"
    );

    assert!(!kernel.in_txn(0), "a failing command aborts the txn");
    assert!(
        !kernel.state().arrays().is_alive(task),
        "command 1 is not applied"
    );
    assert_eq!(
        kernel.last_lsn(),
        lsn_before,
        "the single LSN range is absent — nothing was journaled"
    );
    assert_eq!(digest(kernel.state()), live_before);
    refused(kernel.commit_txn(0), codes::INVALID_KEY);

    kernel.close().expect("close");
    let (reopened, report) = Kernel::open(&dir, options()).expect("reopen");
    // schema txn + the project's creation — nothing of the aborted txn.
    assert_eq!(report.txns_replayed, 2);
    assert_eq!(
        digest(reopened.state()),
        live_before,
        "nothing applied after restart either"
    );
}

/// The happy path: several commands, ONE journal txn, ONE LSN range, and the
/// ids the client saw during the txn are the ids it gets after commit.
#[test]
fn explicit_txn_commits_as_one_journal_txn_with_one_lsn_range() {
    let dir = test_dir("txn_commit");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let lsn_before = kernel.last_lsn();

    kernel.begin_txn(0).expect("begin");
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    set(
        &mut kernel,
        task,
        "title",
        Value::str("write the kernel").unwrap(),
    );
    set(&mut kernel, task, "amount", Value::I64(3));
    // Command 3 references the node command 1 created inside the txn.
    set(&mut kernel, task, "owner", Value::NodeRef(project));
    assert_eq!(
        kernel.last_lsn(),
        lsn_before,
        "nothing journaled before commit"
    );

    let TxnLsns {
        first,
        last,
        records,
    } = kernel.commit_txn(0).expect("commit");
    // BEGIN + 4 records + COMMIT = 6 frames, one contiguous range.
    assert_eq!(records.len(), 4);
    assert_eq!(first, lsn_before + 1);
    assert_eq!(last, first + 5);
    assert_eq!(kernel.last_lsn(), last);

    let txns = journal_txns(&dir);
    // schema txn, the project's auto-commit, then the explicit txn.
    assert_eq!(txns.len(), 3);
    let explicit = txns.last().expect("explicit txn");
    assert_eq!(explicit.0, first);
    assert_eq!(explicit.1, last);
    assert_eq!(explicit.2.len(), 4);

    let live = digest(kernel.state());
    kernel.close().expect("close");
    let (reopened, report) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(
        report.txns_replayed, 3,
        "schema + project + the explicit txn"
    );
    assert_eq!(
        digest(reopened.state()),
        live,
        "replay reproduces the committed txn"
    );
    assert!(reopened.state().arrays().is_alive(task));
    assert_eq!(
        reopened
            .state()
            .machine()
            .latest_cell(task, &key("title"))
            .expect("title")
            .value,
        Value::str("write the kernel").unwrap()
    );
}

/// Rollback discards; nesting and stray commits are refused; a txn whose live
/// state moved underneath it is refused `ORDER_CONFLICT` with nothing applied.
#[test]
fn txn_rollback_nesting_and_conflict() {
    let dir = test_dir("txn_controls");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let live_before = digest(kernel.state());

    // Rollback.
    kernel.begin_txn(0).expect("begin");
    let scratch_node = create(&mut kernel, "task", Some(project.0), vec![]);
    kernel.rollback_txn(0).expect("rollback");
    assert!(!kernel.state().arrays().is_alive(scratch_node));

    // Nested begin.
    kernel.begin_txn(0).expect("begin");
    let nested = kernel.begin_txn(0).expect_err("nested begin");
    assert_eq!(nested.code, codes::TXN_NESTED);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    // Another session writes to the live state while the txn is open.
    set_as(
        &mut kernel,
        1,
        project,
        "title",
        Value::str("moved").unwrap(),
    );
    let conflict = kernel.commit_txn(0).expect_err("commit must conflict");
    assert_eq!(conflict.code, codes::ORDER_CONFLICT);
    assert!(
        !kernel.state().arrays().is_alive(task),
        "nothing of the conflicted txn is applied"
    );

    kernel.begin_txn(0).expect("begin");
    let other = create(&mut kernel, "task", Some(project.0), vec![]);
    let commit = kernel.commit_txn(7).expect_err("not this session's txn");
    assert_eq!(commit.code, codes::LIMIT_EXCEEDED);
    assert!(kernel.in_txn(0), "the txn is still open for its owner");
    kernel.commit_txn(0).expect("owner commits");
    assert!(kernel.state().arrays().is_alive(other));

    // A rolled-back id was never allocated in the journal — the live state
    // hands it out again — so "absent" is about content, not the number.
    assert_eq!(
        kernel.state().arrays().alive_count(),
        2,
        "project + the one committed task; nothing of the rolled-back or conflicted txns"
    );
    let live = digest(kernel.state());
    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(
        digest(reopened.state()),
        live,
        "replay reproduces exactly the committed txns"
    );
    assert_ne!(live, live_before);
}

// -- WPT-7.1 -------------------------------------------------------------------

/// WPT-7.1: ROW_APPEND of 1000 rows across 3 columns with gaps — exactly
/// written cells come back, unaddressed attributes are absent, and the batch
/// is ONE journal record (asserted on the journal itself).
#[test]
fn wpt_7_1_row_append_batch_is_one_journal_record_and_gaps_survive() {
    let dir = test_dir("wpt_7_1");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    let lsn_before = kernel.last_lsn();

    let rows: Vec<Row> = (0..1000u64)
        .map(|i| Row {
            k_skip: if i > 0 && i % 97 == 0 { 3 } else { 0 },
            cells: match i % 3 {
                0 => vec![(key("log"), Value::I64(i as i64))],
                1 => vec![(key("reading"), Value::F64(i as f64 / 4.0))],
                _ => vec![
                    (key("log"), Value::I64(i as i64)),
                    (key("reading"), Value::F64(i as f64 / 4.0)),
                ],
            },
        })
        .collect();
    let reply = kernel
        .execute(
            0,
            &subject(),
            &Command::RowAppend {
                node: task,
                set: key("task_table"),
                rows: rows.clone(),
            },
        )
        .expect("row append");
    // k starts at 0 and advances once per row (+ k_skip holes).
    let mut expected_ks = Vec::with_capacity(rows.len());
    let mut k = 0u64;
    for row in &rows {
        k += u64::from(row.k_skip);
        expected_ks.push(k);
        k += 1;
    }
    assert_eq!(
        reply,
        Reply::Rows {
            node: task,
            first_k: 0,
            count: 1000
        }
    );
    assert_eq!(
        kernel.last_lsn() - lsn_before,
        3,
        "BEGIN + ONE record + COMMIT"
    );
    let txns = journal_txns(&dir);
    // schema txn + two node creations + the row batch
    assert_eq!(txns.len(), 4);
    let batch = txns.last().expect("row batch txn");
    assert_eq!(batch.2.len(), 1, "the whole batch is one journal record");
    assert!(matches!(batch.2[0].data, RecordData::RowAppend { .. }));

    let expect = |attr: &str| {
        let want: Vec<(u64, Value)> = rows
            .iter()
            .zip(&expected_ks)
            .filter_map(|(row, k)| {
                row.cells
                    .iter()
                    .find(|(a, _)| a == &key(attr))
                    .map(|(_, v)| (*k, v.clone()))
            })
            .collect();
        let got = cells_of(&kernel, task, &key(attr));
        assert_eq!(
            got, want,
            "column `{attr}` returns exactly the written cells"
        );
    };
    expect("log");
    expect("reading");
    // An unaddressed attribute is ABSENT at that row's k (no phantom cell);
    // a row addressing both columns has both at the same k.
    let log_ks: Vec<u64> = cells_of(&kernel, task, &key("log"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let reading_ks: Vec<u64> = cells_of(&kernel, task, &key("reading"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for (i, k) in expected_ks.iter().enumerate() {
        let log_only = i % 3 == 0;
        let reading_only = i % 3 == 1;
        let both = i % 3 == 2;
        assert_eq!(log_ks.contains(k), log_only || both, "row {i} at k={k}");
        assert_eq!(
            reading_ks.contains(k),
            reading_only || both,
            "row {i} at k={k}"
        );
    }
    assert_eq!(log_ks.len(), 334 + 333, "log-only rows + both-column rows");
    assert_eq!(
        reading_ks.len(),
        333 + 333,
        "reading-only rows + both-column rows"
    );

    let live = digest(kernel.state());
    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(digest(reopened.state()), live);
}

// -- WPT-7.3 -------------------------------------------------------------------

/// WPT-7.3: DELETE_NODE of a parent — `cascade=false` with live children is
/// `HAS_DEPENDENTS` and deletes nothing; `cascade=true` removes the subtree
/// and the journal shows ordered `DEL_VERTEX` records (children first).
#[test]
fn wpt_7_3_cascade_guard_and_ordered_del_vertex_records() {
    let dir = test_dir("wpt_7_3");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    let subtask = create(&mut kernel, "subtask", Some(task.0), vec![]);
    let live_before = digest(kernel.state());

    let err = refused(delete(&mut kernel, project, false), codes::HAS_DEPENDENTS);
    assert!(err.hint.contains("children"), "{err}");
    assert_eq!(digest(kernel.state()), live_before, "nothing deleted");

    delete(&mut kernel, project, true).expect("cascade delete");
    for node in [project, task, subtask] {
        assert!(!kernel.state().arrays().is_alive(node));
    }
    let txns = journal_txns(&dir);
    let deletes: Vec<u32> = txns
        .last()
        .expect("delete txn")
        .2
        .iter()
        .map(|r| match &r.data {
            RecordData::DelVertex { node } => *node,
            other => panic!("expected DelVertex, got {other:?}"),
        })
        .collect();
    assert_eq!(
        deletes,
        vec![subtask.0, task.0, project.0],
        "children before parents"
    );

    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    for node in [project, task, subtask] {
        assert!(!reopened.state().arrays().is_alive(node));
    }
}

/// With cascade the delete also removes the subtree's dependency edges, and
/// those `DEL_EDGE` records precede the `DEL_VERTEX` ones.
#[test]
fn cascade_delete_removes_incident_edges_first() {
    let dir = test_dir("cascade_edges");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let p1 = create(&mut kernel, "project", None, vec![]);
    let p2 = create(&mut kernel, "project", None, vec![]);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: p1,
                dst: p2,
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    let task = create(&mut kernel, "task", Some(p1.0), vec![]);
    let _ = task;

    delete(&mut kernel, p2, true).expect("cascade delete");
    let txns = journal_txns(&dir);
    let is_del_edge: Vec<bool> = txns
        .last()
        .expect("delete txn")
        .2
        .iter()
        .map(|r| r.kind() == RecordKind::DelEdge)
        .collect();
    assert_eq!(
        is_del_edge,
        vec![true, false],
        "DEL_EDGE precedes DEL_VERTEX"
    );
    // The edge is dead, its slot is kept (ids are never reused).
    assert_eq!(kernel.state().arrays().edge_slots(), 1);
    assert_eq!(kernel.state().arrays().edge_count(), 0);
    assert!(!kernel.state().arrays().is_alive(p2));
}

// -- WPT-7.4 -------------------------------------------------------------------

/// WPT-7.4: ACTION with 2 unmet rules → `REQUIREMENTS_UNMET` + the failing
/// rule keys; after satisfying them the SAME command succeeds — no schema
/// change needed.
#[test]
fn wpt_7_4_action_error_shape_then_success() {
    let dir = test_dir("wpt_7_4");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("ship it").unwrap())],
    );
    action(&mut kernel, task, "start").expect("start");

    let err = refused(
        action(&mut kernel, task, "close"),
        codes::REQUIREMENTS_UNMET,
    );
    assert_eq!(err.hint, "failing rules: amount_gte_10, owner_set");
    assert_eq!(
        state_key(&kernel, task),
        key("running"),
        "no motion on rejection"
    );

    set(&mut kernel, task, "amount", Value::I64(20));
    set(&mut kernel, task, "owner", Value::NodeRef(project));
    let reply = action(&mut kernel, task, "close").expect("same command now succeeds");
    assert_eq!(
        reply,
        Reply::Action {
            node: task,
            from: key("running"),
            to: key("done"),
            spawned: vec![],
            cancelled: vec![],
        }
    );
    assert_eq!(state_key(&kernel, task), key("done"));

    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(state_key(&reopened, task), key("done"));
}

// -- SGT-9.2 / SGT-9.5 ---------------------------------------------------------

/// SGT-9.2 (journal form): requirement met → the node moves, and the journal
/// carries the ACTION record with actor, subject and payload.
#[test]
fn sgt_9_2_state_motion_is_one_journaled_action_record() {
    let dir = test_dir("sgt_9_2");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    // Unmet: no title.
    refused(
        action(&mut kernel, task, "start"),
        codes::REQUIREMENTS_UNMET,
    );
    set(
        &mut kernel,
        task,
        "title",
        Value::str("kernel assembly").unwrap(),
    );
    kernel
        .execute(
            0,
            &subject(),
            &Command::Action {
                node: task,
                transition: key("agent_start"),
                actor: ActorClass::Agent,
                payload: Some(Value::str("go").unwrap()),
            },
        )
        .expect("agent start");

    assert_eq!(state_key(&kernel, task), key("running"));
    let txns = journal_txns(&dir);
    let RecordData::Action {
        node,
        transition,
        actor,
        subject,
        payload,
        from,
        to,
    } = &txns.last().expect("action txn").2[0].data
    else {
        panic!("last record must be an ACTION");
    };
    assert_eq!(*node, task.0);
    assert_eq!(*transition, key("agent_start"));
    assert_eq!(*actor, ActorClass::Agent);
    assert_eq!(subject, "test-subject");
    assert_eq!(payload.clone(), Some(Value::str("go").unwrap()));
    assert_eq!(*from, key("open"));
    assert_eq!(*to, key("running"));
}

/// SGT-9.5 (journal form): the spawn effect creates the child under the
/// actor, typed correctly, and the engine-authored `ADD_VERTEX` is in the
/// SAME journal txn as the ACTION — and replay creates it exactly once.
#[test]
fn sgt_9_5_spawn_effect_journals_engine_add_vertex_in_same_txn() {
    let dir = test_dir("sgt_9_5");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);

    let reply = action(&mut kernel, task, "spawn_sub").expect("spawn");
    let Reply::Action { spawned, .. } = &reply else {
        panic!("expected an Action reply, got {reply:?}");
    };
    assert_eq!(spawned.len(), 1);
    let child = spawned[0];
    let arrays = kernel.state().arrays();
    assert!(arrays.is_alive(child));
    assert_eq!(arrays.parent_of(child), Some(task));
    assert_eq!(
        kernel
            .state()
            .machine()
            .node_type_key(arrays.type_of(child)),
        Some(&key("subtask"))
    );
    assert_eq!(
        state_key(&kernel, child),
        key("live"),
        "a created node starts in its model's initial state"
    );

    let txns = journal_txns(&dir);
    let records = &txns.last().expect("spawn txn").2;
    assert_eq!(
        records.len(),
        2,
        "ACTION + engine-authored ADD_VERTEX in one txn"
    );
    assert!(
        matches!(&records[0].data, RecordData::Action { transition, .. } if *transition == key("spawn_sub"))
    );
    assert!(matches!(
        &records[1].data,
        RecordData::AddVertex {
            node,
            parent: Some(parent),
            node_type,
            authored: lattice_tx::records::Author::Engine,
            ..
        } if *node == child.0 && *parent == task.0 && node_type == &key("subtask")
    ));

    let live = digest(kernel.state());
    let alive = kernel.state().arrays().alive_count();
    kernel.close().expect("close");
    let (reopened, _) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(
        digest(reopened.state()),
        live,
        "replay creates the child exactly once"
    );
    assert_eq!(reopened.state().arrays().alive_count(), alive);
}

// -- SFT-6.4 -------------------------------------------------------------------

/// SFT-6.4: a torn tail is dropped at open, committed state is intact, and
/// new writes continue the LSN sequence without a gap or reuse.
#[test]
fn sft_6_4_append_after_torn_tail_continues_gap_free() {
    let dir = test_dir("sft_6_4");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(
        &mut kernel,
        "project",
        None,
        vec![("title", Value::str("p").unwrap())],
    );
    set(&mut kernel, project, "amount", Value::I64(5));
    let last = kernel.last_lsn();
    let intact = digest(kernel.state());
    kernel.close().expect("close");

    // A frame that was never completed: bytes past the last valid frame.
    let segment = std::fs::read_dir(dir.join("journal"))
        .expect("journal dir")
        .next()
        .expect("one segment")
        .expect("entry")
        .path();
    let torn = [0x05u8, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef, 0x01];
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .expect("open segment")
        .write_all(&torn)
        .expect("append torn frame");

    let (mut reopened, report) = Kernel::open(&dir, options()).expect("reopen");
    assert_eq!(report.torn_tail_dropped, torn.len() as u64);
    assert_eq!(
        report.txns_replayed,
        journal_txns(&dir).len(),
        "every committed txn replays"
    );
    assert_eq!(reopened.last_lsn(), last);
    assert_eq!(digest(reopened.state()), intact);

    // The torn bytes are gone from the file: the next frame starts where the
    // torn one began.
    set(&mut reopened, project, "amount", Value::I64(6));
    assert_eq!(
        reopened.last_lsn(),
        last + 3,
        "BEGIN + record + COMMIT, no gap"
    );
    let after_write = digest(reopened.state());
    reopened.close().expect("close");
    let (again, report) = Kernel::open(&dir, options()).expect("reopen again");
    assert_eq!(report.txns_replayed, journal_txns(&dir).len());
    assert_eq!(report.torn_tail_dropped, 0);
    assert_eq!(digest(again.state()), after_write);
}

// -- cell conformance ----------------------------------------------------------

/// R6 / R7 / R12 through the kernel: type mismatches, undeclared attributes,
/// non-monotone writes and retro appends are refused with nothing written.
#[test]
fn cell_conformance_refusals_leave_the_store_untouched() {
    let dir = test_dir("conformance");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    let epoch_before = kernel.state().arrays().epoch_of(task).0;
    let cells_before = kernel.state().machine().cells().count();

    // SGT-1.1: write with an undeclared attribute key → SCHEMA (R6), DB
    // state unchanged (epoch + node cell count).
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::AttrSet {
                node: task,
                attr: key("colour"),
                value: Value::I64(1),
            },
        ),
        codes::SCHEMA,
    );
    assert!(err.hint.contains("known attributes:"), "{err}");
    assert_eq!(
        kernel.state().arrays().epoch_of(task).0,
        epoch_before,
        "node epoch unchanged"
    );
    assert_eq!(
        kernel.state().machine().cells().count(),
        cells_before,
        "cell count unchanged"
    );
    // Type mismatch.
    refused(
        kernel.execute(
            0,
            &subject(),
            &Command::AttrSet {
                node: task,
                attr: key("title"),
                value: Value::I64(1),
            },
        ),
        codes::SCHEMA,
    );
    // ATTR_SET on a vector column → NOT_APPENDABLE.
    refused(
        kernel.execute(
            0,
            &subject(),
            &Command::AttrSet {
                node: task,
                attr: key("log"),
                value: Value::I64(1),
            },
        ),
        codes::NOT_APPENDABLE,
    );
    // R7: increasing attribute accepts no lower value.
    set(&mut kernel, task, "amount", Value::I64(10));
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::AttrSet {
                node: task,
                attr: key("amount"),
                value: Value::I64(9),
            },
        ),
        codes::NOT_MONOTONE,
    );
    assert!(err.detail.contains("increasing"), "{err}");
    assert_eq!(
        kernel
            .state()
            .machine()
            .latest_cell(task, &key("amount"))
            .expect("amount")
            .value,
        Value::I64(10)
    );
    // R12: k at or below the axis max.
    kernel
        .execute(
            0,
            &subject(),
            &Command::CellsAppend {
                node: task,
                attr: key("log"),
                cells: vec![(0, Value::I64(1)), (1, Value::I64(2))],
            },
        )
        .expect("append");
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::CellsAppend {
                node: task,
                attr: key("log"),
                cells: vec![(1, Value::I64(3))],
            },
        ),
        codes::NOT_APPENDABLE,
    );
    assert!(err.hint.contains("current max k is 1"), "{err}");
    assert_eq!(
        cells_of(&kernel, task, &key("log")),
        vec![(0, Value::I64(1)), (1, Value::I64(2))],
        "existing cells untouched"
    );
}

/// Attribute locking is a consequence of state (SPEC-01 §7/§11, SGT-7.3
/// shape): in the locked state the metadata set is read-only while the table
/// set stays writable.
#[test]
fn state_lock_refuses_writes_to_the_locked_set_only() {
    let dir = test_dir("state_lock");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(
        &mut kernel,
        "sealed_task",
        Some(project.0),
        vec![("title", Value::str("seal me").unwrap())],
    );
    set(&mut kernel, task, "amount", Value::I64(1));
    action(&mut kernel, task, "start").expect("start → running (the locked state)");

    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::AttrSet {
                node: task,
                attr: key("amount"),
                value: Value::I64(2),
            },
        ),
        codes::FORBIDDEN,
    );
    assert!(
        err.hint.contains("state lock"),
        "the hint says it is a lock, not a grant: {err}"
    );
    assert_eq!(
        kernel
            .state()
            .machine()
            .latest_cell(task, &key("amount"))
            .expect("amount")
            .value,
        Value::I64(1),
        "the locked write did not land"
    );
    kernel
        .execute(
            0,
            &subject(),
            &Command::CellsAppend {
                node: task,
                attr: key("log"),
                cells: vec![(0, Value::I64(7))],
            },
        )
        .expect("the table set stays writable");
}

/// node_ref integrity (SPEC-01 §3): a referenced node cannot be deleted —
/// with or without cascade — while the reference lives.
#[test]
fn node_ref_target_cannot_be_deleted_while_referenced() {
    let dir = test_dir("node_ref");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let owner = create(&mut kernel, "project", None, vec![]);
    let other = create(&mut kernel, "project", None, vec![]);
    // The referencing node sits OUTSIDE the owner's subtree.
    let task = create(
        &mut kernel,
        "task",
        Some(other.0),
        vec![("owner", Value::NodeRef(owner))],
    );
    for cascade in [false, true] {
        let err = refused(delete(&mut kernel, owner, cascade), codes::HAS_DEPENDENTS);
        assert!(
            err.hint.contains("node_ref"),
            "cascade does not override a live reference: {err}"
        );
    }
    assert!(kernel.state().arrays().is_alive(owner));
    // Point the reference elsewhere, then the owner goes.
    set(&mut kernel, task, "owner", Value::NodeRef(other));
    delete(&mut kernel, owner, true).expect("delete once unreferenced");
    assert!(!kernel.state().arrays().is_alive(owner));
}

// -- structure -----------------------------------------------------------------

/// Containment (R2), unknown names (hint law), dependency cycles (R3) and
/// state locks are refused before anything is journaled.
#[test]
fn structure_refusals_follow_the_hint_law() {
    let dir = test_dir("hints");
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    let lsn_before = kernel.last_lsn();
    let project = create(&mut kernel, "project", None, vec![]);
    let other = create(&mut kernel, "project", None, vec![]);

    // Containment: a task cannot nest under another task's parent slot.
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("pm"),
                node_type: key("subtask"),
                parent: Some(project),
                attrs: vec![],
            },
        ),
        codes::CONTAINMENT,
    );
    assert!(err.hint.contains("requires parent type `task`"), "{err}");
    // A child type created as a root.
    refused(
        kernel.execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("pm"),
                node_type: key("task"),
                parent: None,
                attrs: vec![],
            },
        ),
        codes::CONTAINMENT,
    );
    // Unknown graph / type.
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("no_graph"),
                node_type: key("project"),
                parent: None,
                attrs: vec![],
            },
        ),
        codes::NOT_FOUND,
    );
    assert!(err.hint.contains("known graphs: pm"), "{err}");
    // R3: a→b then b→a closes a cycle.
    let a = create(&mut kernel, "task", Some(project.0), vec![]);
    let b = create(&mut kernel, "task", Some(other.0), vec![]);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: a,
                dst: b,
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link a→b");
    let err = refused(
        kernel.execute(
            0,
            &subject(),
            &Command::Link {
                src: b,
                dst: a,
                kind: kinds::OR,
                aux: 0,
                guard: None,
            },
        ),
        codes::CYCLE,
    );
    // a → b exists; b → a would close it: the hint walks the offending path.
    assert_eq!(err.hint, format!("offending path: {} → {}", a.0, b.0));
    assert_eq!(kernel.state().arrays().edge_count(), 1);
    assert_eq!(
        kernel.last_lsn(),
        lsn_before + 5 * 3,
        "only the accepted commands were journaled"
    );
}
