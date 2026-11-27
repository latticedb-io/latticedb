//! LatticeDB validation: rules R1–R13 and kernel scope checks (R11) — the
//! unbypassable in-transaction checks (P9). Validation happens at the
//! sequencing point (ARCHITECTURE §5.1).
//!
//! This crate is the explicit kernel-gate façade: pure functions over the
//! compiled catalog / arrays / cells. The engine's dispatch (lattice-engine)
//! and the txn layer (lattice-tx) delegate here — one gate set, one place.
//!
//! Rule map (v1 gate set):
//! - R1  edge params: per-kind validation (KOFN range, WEIGHTED
//!   normalization, GUARD reference resolution) — delegates to algebra.
//! - R2  containment: child type's parent_type must match the parent node's
//!   type; composition edges stay inside their graph.
//! - R3  dependency acyclicity: the dependency DAG is acyclic per Database
//!   (global, cross-graph — SGT-15.3).
//! - R4  backward rank: finalizing motions strictly increase rank.
//! - R5  terminal absorption: no motion out of a terminal state.
//! - R6  write schema conformance: attribute declared, value type matches,
//!   node_ref targets an alive node in the same Database.
//! - R7  monotone writes: attributes with `increasing|decreasing` accept
//!   only direction-conforming overwrites (structural guarantee behind
//!   predicate monotonicity — micro-decision 7).
//! - R8  enum bounds: ≤ 256 unique values (SPEC-01 §4).
//! - R9  record bounds: flat, 1–64 fields, unique names (SPEC-01 §5).
//! - R10 key grammar: `^[a-z][a-z0-9_]{0,63}$` (SPEC-01 §2).
//! - R11 kernel scope: the acting subject's scope set covers the required
//!   scope for the operation (in-transaction, journaled — P9).
//! - R12 append-only vector axis: cell writes to k below the current max
//!   are rejected NOT_APPENDABLE (SPEC-01 §6; corrections = re-run).
//! - R13 guarded evolution: reference-counted schema ops (SPEC-01 §16 —
//!   implemented in lattice-schema::txn, surfaced here as the rule id).

pub mod rules;

pub use rules::{RequiredScope, Validators, Violation};
