//! LatticeDB schema: catalog entities (SPEC-01 §2–§15) and the schema compiler —
//! dispatch masks, requirement read-sets, enabled-action index, index-def plans
//! (ARCHITECTURE §7). Schema transactions with R13 reference-count validation
//! live here (SPEC-01 §16).

pub mod catalog;
pub mod compile;
pub mod entities;
pub mod key;
pub mod opslog;
pub mod txn;
