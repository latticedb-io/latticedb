//! T-313 acceptance, part 2 — recovery and determinism of the assembled
//! kernel (ARCHITECTURE §5.3, SPEC-03 §1 D2):
//!
//! - SFT-1.1 (kernel form) — a seeded random command log through the kernel:
//!   live state == replayed state, and three fresh copies are byte-identical.
//! - SFT-3.1 — write → checkpoint → reopen: structure from the snapshot,
//!   values from parts, history from the journal; every column type and
//!   holes in the key axis survive.
//! - Lineage (BACKLOG ED-14): attribute ids and node-type codes are rebuildable
//!   from the op history, so a restart never re-keys a cell or re-types a node.
//! - GUARD params (ED-15) survive restarts; guard ids and edge ids are never
//!   reused.
//! - Watch events leave the journal append point in journal order.

mod common;

use common::*;
use lattice_core::ids::{EdgeId, NodeId};
use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_engine::dispatch::kinds;
use lattice_schema::entities::{
    ActorClass, AttrType, AttributeDef, Monotonicity, NodeType, ScalarType,
};
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;
use lattice_tx::{Command, EventKind, Kernel, Reply, Row, codes};
use std::path::{Path, PathBuf};

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

fn fresh(dir: &Path) -> Kernel {
    let mut kernel = Kernel::create(dir, 1, limits()).expect("create database");
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install schema");
    kernel
}

fn reopen(dir: &Path) -> Kernel {
    Kernel::open(dir, limits()).expect("reopen").0
}

// -- SFT-1.1 (kernel form) ------------------------------------------------------

/// xorshift64* — the seeded generator (same seed ⇒ same command log on any
/// machine; SPEC-03 D4 keeps it out of stored bytes).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the log created and may reference later.
#[derive(Default)]
struct World {
    projects: Vec<NodeId>,
    tasks: Vec<NodeId>,
    subtasks: Vec<NodeId>,
    /// Last `amount` per node — writes stay monotone by construction.
    amounts: std::collections::BTreeMap<u32, i64>,
}

impl World {
    fn all(&self) -> Vec<NodeId> {
        let mut v = self.projects.clone();
        v.extend(&self.tasks);
        v.extend(&self.subtasks);
        v
    }
}

fn pick(rng: &mut Rng, nodes: &[NodeId]) -> NodeId {
    nodes[rng.below(nodes.len() as u64) as usize]
}

/// One random command over the world so far.
fn sample(rng: &mut Rng, world: &World) -> Command {
    let all = world.all();
    match rng.below(100) {
        0..=5 => Command::CreateNode {
            graph: key("pm"),
            node_type: key("project"),
            parent: None,
            attrs: vec![],
        },
        6..=17 => Command::CreateNode {
            graph: key("pm"),
            node_type: key("task"),
            parent: Some(pick(rng, &world.projects)),
            attrs: vec![],
        },
        18..=25 if !world.tasks.is_empty() => Command::CreateNode {
            graph: key("pm"),
            node_type: key("subtask"),
            parent: Some(pick(rng, &world.tasks)),
            attrs: vec![],
        },
        26..=37 => Command::AttrSet {
            node: pick(rng, &all),
            attr: key("title"),
            value: Value::str(format!("t{}", rng.below(97))).unwrap(),
        },
        38..=49 => {
            let node = pick(rng, &world.tasks);
            let value =
                world.amounts.get(&node.0).copied().unwrap_or(0) + 1 + (rng.below(17) as i64);
            Command::AttrSet {
                node,
                attr: key("amount"),
                value: Value::I64(value),
            }
        }
        50..=55 => Command::AttrSet {
            node: pick(rng, &all),
            attr: key("prio"),
            value: Value::Enum(rng.below(2) as u8),
        },
        56..=61 => Command::AttrSet {
            node: pick(rng, &world.tasks),
            attr: key("owner"),
            value: Value::NodeRef(pick(rng, &world.projects)),
        },
        62..=69 => Command::RowAppend {
            node: pick(rng, &world.tasks),
            set: key("task_table"),
            rows: (0..1 + rng.below(3))
                .map(|i| Row {
                    k_skip: if rng.below(7) == 0 { 2 } else { 0 },
                    cells: match rng.below(3) {
                        0 => vec![(key("log"), Value::I64(i as i64))],
                        1 => vec![(key("reading"), Value::F64(i as f64))],
                        _ => vec![
                            (key("log"), Value::I64(i as i64)),
                            (key("reading"), Value::F64(i as f64)),
                        ],
                    },
                })
                .collect(),
        },
        70..=79 => Command::Action {
            node: pick(rng, &world.tasks),
            transition: key(match rng.below(5) {
                0 => "start",
                1 => "fail",
                2 => "complete",
                3 => "close",
                _ => "spawn_sub",
            }),
            actor: if rng.below(2) == 0 {
                ActorClass::Human
            } else {
                ActorClass::Agent
            },
            payload: None,
        },
        80..=89 => Command::Link {
            src: pick(rng, &all),
            dst: pick(rng, &all),
            kind: 1 + rng.below(3) as u8,
            aux: 0,
            guard: None,
        },
        90..=94 => Command::Unlink {
            edge: rng.below(64),
        },
        _ if !world.subtasks.is_empty() => Command::DeleteNode {
            node: pick(rng, &world.subtasks),
            cascade: rng.below(2) == 0,
        },
        _ => Command::AttrSet {
            node: pick(rng, &all),
            attr: key("title"),
            value: Value::str("again").unwrap(),
        },
    }
}

/// Run a seeded command log through a fresh database; returns the live
/// digest and the number of accepted commands. Refusals are part of the
/// workload (a real client hits them); a kernel fault is not — it fails.
fn run_log(dir: &Path, seed: u64, ops: usize) -> (String, usize) {
    let mut kernel = fresh(dir);
    let mut rng = Rng(seed | 1);
    let mut world = World::default();
    // A deterministic head so every sampler arm has something to pick.
    for _ in 0..2 {
        let cmd = Command::CreateNode {
            graph: key("pm"),
            node_type: key("project"),
            parent: None,
            attrs: vec![],
        };
        match kernel.execute(0, &subject(), &cmd).expect("head project") {
            Reply::Created(id) => world.projects.push(id),
            other => panic!("{other:?}"),
        }
    }
    for _ in 0..3 {
        let cmd = Command::CreateNode {
            graph: key("pm"),
            node_type: key("task"),
            parent: Some(world.projects[0]),
            attrs: vec![(key("title"), Value::str("head").unwrap())],
        };
        match kernel.execute(0, &subject(), &cmd).expect("head task") {
            Reply::Created(id) => world.tasks.push(id),
            other => panic!("{other:?}"),
        }
    }
    let mut accepted = 0usize;
    let mut refused = 0usize;
    for _ in 0..ops {
        let cmd = sample(&mut rng, &world);
        let outcome = kernel.execute(0, &subject(), &cmd);
        if let Err(err) = &outcome {
            assert_ne!(
                err.code,
                codes::INTERNAL,
                "a kernel fault is never part of the workload: {err}"
            );
            refused += 1;
        } else {
            accepted += 1;
        }
        match (&outcome, &cmd) {
            (Ok(Reply::Created(id)), Command::CreateNode { node_type, .. }) => {
                if node_type == &key("project") {
                    world.projects.push(*id);
                } else if node_type == &key("task") {
                    world.tasks.push(*id);
                } else {
                    world.subtasks.push(*id);
                }
            }
            (
                Ok(Reply::Attr { node }),
                Command::AttrSet {
                    attr,
                    value: Value::I64(v),
                    ..
                },
            ) => {
                if attr == &key("amount") {
                    world.amounts.insert(node.0, *v);
                }
            }
            (Ok(_), Command::DeleteNode { node, .. }) => world.subtasks.retain(|n| n != node),
            _ => {}
        }
    }
    assert!(
        accepted > ops / 2,
        "the log is mostly real work ({accepted} accepted, {refused} refused)"
    );
    let live = digest(kernel.state());
    kernel.close().expect("close");
    (live, accepted)
}

fn segment_bytes(dir: &Path) -> Vec<Vec<u8>> {
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir.join("journal"))
        .expect("journal dir")
        .map(|e| e.expect("entry").path())
        .collect();
    segments.sort();
    segments
        .iter()
        .map(|p| std::fs::read(p).expect("segment"))
        .collect()
}

/// The durable image: snapshot bytes, part bytes by id, the MANIFEST.
type DurableImage = (Vec<u8>, Vec<(u64, Vec<u8>)>, String);

fn durable_image(dir: &Path, kernel: &Kernel) -> DurableImage {
    let manifest = kernel.manifest();
    let snapshot =
        std::fs::read(dir.join(format!("snapshot.v{}", manifest.snapshot_id))).expect("snapshot");
    let parts = manifest
        .parts
        .iter()
        .map(|id| {
            (
                *id,
                std::fs::read(dir.join("parts").join(format!("{id:012}.lpart"))).expect("part"),
            )
        })
        .collect();
    (snapshot, parts, format!("{manifest:?}"))
}

/// SFT-1.1 (kernel form): one seeded command log on three fresh databases →
/// three identical journals, byte-identical snapshots and parts after a
/// checkpoint; and in each, live state == replayed state.
#[test]
fn sft_1_1_kernel_form_replay_identity_and_three_fresh_copies() {
    for seed in [0x5EED_F00Du64, 0xC0FF_EE42] {
        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| test_dir(&format!("sft_1_1_{seed:x}_{i}")))
            .collect();
        let mut digests = Vec::new();
        let mut journals = Vec::new();
        let mut images: Vec<DurableImage> = Vec::new();
        for dir in &dirs {
            let (live, _) = run_log(dir, seed, 400);
            let (mut reopened, report) = Kernel::open(dir, limits()).expect("reopen");
            assert!(report.txns_replayed > 0);
            assert_eq!(
                digest(reopened.state()),
                live,
                "live state == replay(journal), seed {seed:x}"
            );
            journals.push(segment_bytes(dir));
            reopened.checkpoint().expect("checkpoint");
            reopened.close().expect("close");
            let (again, report) = Kernel::open(dir, limits()).expect("reopen after checkpoint");
            assert_eq!(report.txns_replayed, 0, "the snapshot covers everything");
            assert_eq!(digest(again.state()), live, "snapshot + parts == live");
            digests.push(live);
            images.push(durable_image(dir, &again));
        }
        assert_eq!(digests[0], digests[1]);
        assert_eq!(digests[1], digests[2]);
        assert_eq!(journals[0], journals[1], "same log ⇒ same journal bytes");
        assert_eq!(journals[1], journals[2]);
        assert_eq!(
            images[0].0, images[1].0,
            "same log ⇒ identical snapshot bytes"
        );
        assert_eq!(images[0].1, images[1].1, "same log ⇒ identical part bytes");
        assert_eq!(images[0].2, images[1].2, "same log ⇒ identical MANIFEST");
    }
}

/// A different log is a different database (the oracle is not vacuous).
#[test]
fn sft_1_1_different_logs_diverge() {
    let (a, _) = run_log(&test_dir("sft_1_1_a"), 1, 200);
    let (b, _) = run_log(&test_dir("sft_1_1_b"), 2, 200);
    assert_ne!(a, b);
}

// -- SFT-3.1 ---------------------------------------------------------------------

/// One cell of every column type, plus rows with holes in the key axis.
fn write_every_column_type(kernel: &mut Kernel, task: NodeId) {
    set(kernel, task, "title", Value::str("every type").unwrap());
    set(kernel, task, "amount", Value::I64(-9_000_000_000));
    set(
        kernel,
        task,
        "budget",
        Value::Decimal(Decimal::new(-12_345, 10, 2).unwrap()),
    );
    set(
        kernel,
        task,
        "due_at",
        Value::Timestamp(1_700_000_000_000_000),
    );
    set(kernel, task, "sla", Value::Duration(-5));
    set(
        kernel,
        task,
        "blob",
        Value::bytes(vec![0u8, 1, 255, 7]).unwrap(),
    );
    set(kernel, task, "prio", Value::Enum(1));
    set(
        kernel,
        task,
        "evidence",
        Value::Record(
            RecordValue::from_pairs(&[(0, Value::I64(9)), (1, Value::str("n").unwrap())]).unwrap(),
        ),
    );
    kernel
        .execute(
            0,
            &subject(),
            &Command::RowAppend {
                node: task,
                set: key("task_table"),
                rows: vec![
                    Row {
                        k_skip: 0,
                        cells: vec![
                            (key("log"), Value::I64(1)),
                            (key("reading"), Value::F64(1.5)),
                        ],
                    },
                    Row {
                        k_skip: 4,
                        cells: vec![(key("reading"), Value::F64(2.5))],
                    },
                    Row {
                        k_skip: 0,
                        cells: vec![(key("log"), Value::I64(3))],
                    },
                ],
            },
        )
        .expect("rows with holes");
}

/// SFT-3.1 three-layer truth: write → checkpoint → reopen gives structure
/// from the snapshot, values from parts, history from the journal — every
/// column type and the key-axis holes intact — and writing continues.
#[test]
fn sft_3_1_checkpoint_reopen_restores_every_layer_exactly() {
    let dir = test_dir("sft_3_1");
    let mut kernel = fresh(&dir);
    let project = create(
        &mut kernel,
        "project",
        None,
        vec![("title", Value::str("p").unwrap())],
    );
    let task = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("owner", Value::NodeRef(project))],
    );
    write_every_column_type(&mut kernel, task);
    kernel
        .execute(
            0,
            &subject(),
            &Command::Link {
                src: task,
                dst: project,
                kind: kinds::AND,
                aux: 0,
                guard: None,
            },
        )
        .expect("link");
    let live = digest(kernel.state());

    let report = kernel.checkpoint().expect("checkpoint");
    assert_eq!(report.created_lsn, kernel.last_lsn());
    assert_eq!(report.parts, kernel.manifest().parts.len());
    assert!(report.cells > 0);
    assert!(
        dir.join(format!("snapshot.v{}", report.snapshot_id))
            .is_file()
    );
    for id in &kernel.manifest().parts {
        assert!(
            dir.join("parts").join(format!("{id:012}.lpart")).is_file(),
            "MANIFEST-listed part {id} exists"
        );
    }
    assert_eq!(
        digest(kernel.state()),
        live,
        "a checkpoint does not change logical state"
    );
    kernel.close().expect("close");

    let (mut reopened, open) = Kernel::open(&dir, limits()).expect("reopen");
    assert_eq!(
        open.txns_replayed, 0,
        "history before the checkpoint sits in snapshot + parts"
    );
    assert_eq!(
        digest(reopened.state()),
        live,
        "structure + values restored exactly"
    );
    let machine = reopened.state().machine();
    let expect = |attr: &str, value: Value| {
        assert_eq!(
            machine.latest_cell(task, &key(attr)).expect(attr).value,
            value
        );
    };
    expect("title", Value::str("every type").unwrap());
    expect("amount", Value::I64(-9_000_000_000));
    expect(
        "budget",
        Value::Decimal(Decimal::new(-12_345, 10, 2).unwrap()),
    );
    expect("due_at", Value::Timestamp(1_700_000_000_000_000));
    expect("sla", Value::Duration(-5));
    expect("blob", Value::bytes(vec![0u8, 1, 255, 7]).unwrap());
    expect("prio", Value::Enum(1));
    expect("owner", Value::NodeRef(project));
    // The key axis kept its hole: rows at k = 0, 5, 6.
    let log_id = machine.compiled_ref().attr_ids[&key("log")];
    let ks: Vec<u64> = machine
        .cells()
        .filter(|((n, a, _), _)| *n == task && *a == log_id)
        .map(|((_, _, k), _)| k.0)
        .collect();
    assert_eq!(ks, vec![0, 6]);

    // Writing continues; the next open replays only the new history.
    set(&mut reopened, task, "amount", Value::I64(5));
    let lsn = reopened.last_lsn();
    let after = digest(reopened.state());
    reopened.close().expect("close");
    let (again, open) = Kernel::open(&dir, limits()).expect("reopen again");
    assert_eq!(open.txns_replayed, 1, "exactly the post-checkpoint txn");
    assert_eq!(open.last_lsn, lsn);
    assert_eq!(digest(again.state()), after);
}

// -- lineage (ED-14) -------------------------------------------------------------

fn scalar_attr(k: &str) -> SchemaOp {
    SchemaOp::CreateAttribute(AttributeDef {
        key: key(k),
        attr_type: AttrType::Scalar(ScalarType::I64),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    })
}

fn root_type(k: &str, sets: Vec<&str>) -> SchemaOp {
    SchemaOp::CreateNodeType(NodeType {
        key: key(k),
        display_name: k.to_string(),
        parent_type: None,
        state_model: key("m"),
        attribute_sets: sets.into_iter().map(key).collect(),
        key_type: key("ordinal"),
        state_locks: vec![],
    })
}

fn set_of(k: &str, members: Vec<&str>) -> SchemaOp {
    SchemaOp::CreateAttributeSet(lattice_schema::entities::AttributeSet {
        key: key(k),
        kind: lattice_schema::entities::SetKind::ScalarSet,
        members: members.into_iter().map(key).collect(),
        write_scope: None,
    })
}

/// Attribute ids and node-type codes come from the op history, not from the
/// catalog's sort order: declaring `alpha` and `aaa` AFTER `zebra` and `zeta`
/// (both sort first) neither re-keys a cell nor re-types a node — in-process
/// or across a restart from a checkpoint.
#[test]
fn lineage_of_ids_and_type_codes_survives_schema_growth_and_restart() {
    let dir = test_dir("lineage");
    let mut kernel = Kernel::create(&dir, 1, limits()).expect("create database");
    let epoch1 = vec![
        SchemaOp::CreateGraph(lattice_schema::entities::GraphSchema {
            key: key("g"),
            display_name: "G".into(),
            created_at_us: 0,
        }),
        SchemaOp::CreateKeyType(lattice_schema::entities::KeyTypeDef {
            key: key("ordinal"),
            density: lattice_schema::entities::Density::Dense,
            axis: lattice_schema::entities::AxisKind::Ordinal,
        }),
        SchemaOp::CreateStateModel(
            lattice_schema::entities::StateModel::new(
                key("m"),
                lattice_schema::entities::ModelClass::Endless,
                vec![state("live", "Live", false, false)],
                key("live"),
            )
            .unwrap(),
        ),
        scalar_attr("zebra"),
        set_of("s_zebra", vec!["zebra"]),
        root_type("zeta", vec!["s_zebra"]),
    ];
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: epoch1 })
        .expect("epoch 1");
    let zeta = match kernel
        .execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("g"),
                node_type: key("zeta"),
                parent: None,
                attrs: vec![],
            },
        )
        .expect("zeta node")
    {
        Reply::Created(id) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(kernel.state().schema.node_type_codes()[&key("zeta")], 0);

    let epoch2 = vec![
        scalar_attr("alpha"),
        set_of("s_both", vec!["zebra", "alpha"]),
        root_type("aaa", vec!["s_both"]),
    ];
    kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: epoch2 })
        .expect("epoch 2");

    // In-process: the recompile kept zeta's code and zebra's id.
    let codes = kernel.state().schema.node_type_codes();
    assert_eq!(codes[&key("zeta")], 0, "first appearance, not sort order");
    assert_eq!(codes[&key("aaa")], 1);
    let ids = kernel.state().machine().compiled_ref().attr_ids.clone();
    assert!(
        ids[&key("alpha")] > ids[&key("zebra")],
        "attr ids follow first appearance too"
    );
    assert_eq!(
        kernel
            .state()
            .machine()
            .node_type_key(kernel.state().arrays().type_of(zeta)),
        Some(&key("zeta")),
        "the existing node kept its type"
    );

    let aaa = match kernel
        .execute(
            0,
            &subject(),
            &Command::CreateNode {
                graph: key("g"),
                node_type: key("aaa"),
                parent: None,
                attrs: vec![],
            },
        )
        .expect("aaa node")
    {
        Reply::Created(id) => id,
        other => panic!("{other:?}"),
    };
    set(&mut kernel, aaa, "alpha", Value::I64(1));
    set(&mut kernel, aaa, "zebra", Value::I64(2));
    let live = digest(kernel.state());
    kernel.checkpoint().expect("checkpoint");
    kernel.close().expect("close");

    let reopened = reopen(&dir);
    assert_eq!(
        digest(reopened.state()),
        live,
        "ids and codes rebuild identically"
    );
    assert_eq!(
        reopened.state().machine().compiled_ref().attr_ids.clone(),
        ids
    );
    assert_eq!(
        reopened
            .state()
            .machine()
            .node_type_key(reopened.state().arrays().type_of(zeta)),
        Some(&key("zeta"))
    );
    assert_eq!(
        reopened
            .state()
            .machine()
            .latest_cell(aaa, &key("alpha"))
            .expect("alpha")
            .value,
        Value::I64(1),
        "the cell kept its attribute"
    );
}

// -- guards, ids -----------------------------------------------------------------

fn guard_edge(src: NodeId, dst: NodeId) -> Command {
    Command::Link {
        src,
        dst,
        kind: kinds::GUARD,
        aux: 0,
        // Sigma `running`: the guard arms once the SOURCE reaches it, then
        // fires `auto_done` (from-state `running`) on the target.
        guard: Some((key("running"), key("auto_done"))),
    }
}

fn outgoing_auxes(kernel: &Kernel, node: NodeId) -> Vec<u32> {
    kernel
        .state()
        .arrays()
        .dep_out_ids(node)
        .iter()
        .filter_map(|e| kernel.state().arrays().edge(*e))
        .map(|r| r.aux)
        .collect()
}

/// GUARD params live outside the snapshot format: the journal rebuilds them
/// at open, before and after a checkpoint. Guard aux ids and edge ids are
/// never reused.
#[test]
fn guard_params_survive_restart_and_ids_are_never_reused() {
    let dir = test_dir("guards");
    let mut kernel = fresh(&dir);
    let project = create(
        &mut kernel,
        "project",
        None,
        vec![("title", Value::str("p").unwrap())],
    );
    let task = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("t").unwrap())],
    );
    let other = create(
        &mut kernel,
        "task",
        Some(project.0),
        vec![("title", Value::str("o").unwrap())],
    );
    let _ = other;

    kernel
        .execute(0, &subject(), &guard_edge(project, task))
        .expect("guard 1");
    let first_aux = outgoing_auxes(&kernel, project)[0];
    // Unlink, then link again: a NEW edge id and a NEW aux id.
    kernel
        .execute(0, &subject(), &Command::Unlink { edge: 0 })
        .expect("unlink");
    kernel
        .execute(0, &subject(), &guard_edge(project, task))
        .expect("guard 2");
    let arrays = kernel.state().arrays();
    assert_eq!(arrays.edge_slots(), 2, "slot 0 is kept, dead");
    assert!(!arrays.edge_slot(EdgeId(0)).expect("slot 0").alive);
    assert!(arrays.edge(EdgeId(1)).expect("slot 1 is live").alive);
    assert_ne!(
        outgoing_auxes(&kernel, project)[0],
        first_aux,
        "aux ids are never reused"
    );
    let live = digest(kernel.state());
    kernel.close().expect("close");

    // Journal replay (no checkpoint): the guard is armed on the restart.
    let mut reopened = reopen(&dir);
    assert_eq!(digest(reopened.state()), live);
    fire_guard(&mut reopened, project, task);

    // Checkpoint, reopen: params now come from the PRE-checkpoint journal.
    let before = digest(reopened.state());
    reopened.checkpoint().expect("checkpoint");
    reopened.close().expect("close");
    let reopened = reopen(&dir);
    assert_eq!(digest(reopened.state()), before);
    assert_eq!(
        reopened
            .state()
            .machine()
            .guard_tau_for(project, task, &key("running")),
        Some(key("auto_done")),
        "guard params restored from the journal"
    );
}

/// Source and target reach `running`; the source's arrival fires the guard,
/// which moves the target to `done` through the engine-armed transition.
fn fire_guard(kernel: &mut Kernel, source: NodeId, target: NodeId) {
    for node in [target, source] {
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
            .expect("start");
    }
    assert_eq!(
        kernel.state().machine().state_key_of(target).unwrap(),
        key("done"),
        "the GUARD fired on the restart-rebuilt params"
    );
}

// -- watches ---------------------------------------------------------------------

/// Events leave the journal append point in journal order, stamped with the
/// committing LSN, and `action_enabled` / `action_disabled` flip exactly when
/// the enabled-action index flips.
#[test]
fn watch_events_follow_journal_order_and_enabled_flips() {
    let dir = test_dir("watches");
    let mut kernel = fresh(&dir);
    let lsn0 = kernel.last_lsn();
    let project = create(&mut kernel, "project", None, vec![]);
    let task = create(&mut kernel, "task", Some(project.0), vec![]);
    let lsn_created = kernel.last_lsn();

    // No title yet: only the requirement-free `spawn_sub` is enabled.
    assert_eq!(
        kernel.state().machine().enabled_actions(task).unwrap(),
        vec![key("spawn_sub")]
    );
    set(&mut kernel, task, "title", Value::str("go").unwrap());
    let lsn_title = kernel.last_lsn();
    assert_eq!(
        kernel.state().machine().enabled_actions(task).unwrap(),
        vec![key("agent_start"), key("spawn_sub"), key("start")]
    );
    kernel
        .execute(
            0,
            &subject(),
            &Command::Action {
                node: task,
                transition: key("start"),
                actor: ActorClass::Human,
                payload: None,
            },
        )
        .expect("start");
    let lsn_action = kernel.last_lsn();
    assert_eq!(
        kernel.state().machine().enabled_actions(task).unwrap(),
        vec![key("fail")]
    );

    let events = kernel.watch_events_since(lsn0, 10_000);
    assert_eq!(kernel.watch_events_dropped(), 0);
    let lsns: Vec<u64> = events.iter().map(|e| e.lsn).collect();
    assert!(lsns.windows(2).all(|w| w[0] <= w[1]), "journal order");
    assert!(lsns.iter().all(|l| *l > lsn0 && *l <= lsn_action));

    let created: Vec<_> = events
        .iter()
        .filter(|e| e.lsn == lsn_created)
        .map(|e| (e.event.kind, e.event.node))
        .collect();
    // A created node enters its initial state, joins the structure, and its
    // requirement-free action shows up in the enabled index at once.
    assert_eq!(
        created,
        vec![
            (EventKind::State, task),
            (EventKind::Struct, task),
            (EventKind::Enabled, task)
        ]
    );

    let flips_at = |lsn: u64| -> Vec<(bool, Key)> {
        events
            .iter()
            .filter(|e| e.lsn == lsn && e.event.kind == EventKind::Enabled)
            .map(|e| {
                (
                    e.event.enabled.expect("enabled flag"),
                    e.event.transition.clone().expect("transition"),
                )
            })
            .collect()
    };
    // The title write turned both start transitions on.
    assert_eq!(
        flips_at(lsn_title),
        vec![(true, key("agent_start")), (true, key("start"))]
    );
    // The action turned every `open`-transition off and `fail` on — the
    // index now reads exactly [fail].
    assert_eq!(
        flips_at(lsn_action),
        vec![
            (true, key("fail")),
            (false, key("agent_start")),
            (false, key("spawn_sub")),
            (false, key("start"))
        ]
    );
    assert!(events.iter().any(|e| e.lsn == lsn_action
        && e.event.kind == EventKind::Action
        && e.event.transition.as_ref() == Some(&key("start"))));
    // Paging: the cursor is the last LSN the caller has seen.
    assert_eq!(
        kernel.watch_events_since(lsn_action, 10_000).len(),
        0,
        "nothing after the last unit"
    );
    assert!(
        kernel
            .watch_events_since(lsn_title, 10_000)
            .iter()
            .all(|e| e.lsn == lsn_action)
    );
}
