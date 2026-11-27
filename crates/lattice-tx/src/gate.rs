//! Machine-backed sequencer gate: validates node-locally at sequencing
//! time (R7 monotone overwrites, R12 append-only, containment/dead-node)
//! and applies the fact to the engine (the state the journal replays into).

use crate::sequencer::SequencerGate;
use crate::{Fact, FactPayload, TxError};
use lattice_core::ids::{AttrId, KeyIndex, NodeId};
use lattice_core::value::Value;
use lattice_engine::dispatch::Machine;
use lattice_schema::entities::Monotonicity;
use lattice_schema::key::Key;

/// Machine gate: validation + application in one sequencing-time step.
pub struct MachineGate {
    machine: Machine,
    /// attr key → monotonicity (from the catalog; R7 enforcement).
    monotone: std::collections::BTreeMap<Key, Monotonicity>,
    /// attr key → append-only axis (R12 enforcement).
    append_only: std::collections::BTreeSet<Key>,
}

impl MachineGate {
    pub fn new(
        machine: Machine,
        monotone: std::collections::BTreeMap<Key, Monotonicity>,
        append_only: std::collections::BTreeSet<Key>,
    ) -> Self {
        Self {
            machine,
            monotone,
            append_only,
        }
    }

    /// Resolve an AttrId to its attr key via the compiled catalog.
    fn attr_key(&self, attr: AttrId) -> Option<Key> {
        self.machine
            .compiled_ref()
            .attr_ids
            .iter()
            .find(|(_, id)| **id == attr)
            .map(|(k, _)| k.clone())
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }
}

impl SequencerGate for MachineGate {
    fn validate_and_apply(&mut self, fact: &Fact) -> Result<(), TxError> {
        match &fact.payload {
            FactPayload::AttrSet {
                node,
                attr,
                k,
                value,
            } => {
                let Some(key) = self.attr_key(*attr) else {
                    return Err(TxError::Validation {
                        rule: format!("attr {} not in catalog", attr.0),
                    });
                };
                self.check_target_alive(*node)?;
                self.check_monotone(*node, &key, *k, value)?;
                self.machine
                    .set_cell(*node, &key, *k, value.clone(), fact.txn_id.into())
                    .map_err(gate_err)?;
                Ok(())
            }
            FactPayload::CellsAppend {
                node,
                attr,
                k,
                value,
            } => {
                let Some(key) = self.attr_key(*attr) else {
                    return Err(TxError::Validation {
                        rule: format!("attr {} not in catalog", attr.0),
                    });
                };
                self.check_target_alive(*node)?;
                // R12: append-only axis — k must be above the current max.
                if self.append_only.contains(&key)
                    && let Some(max_k) = self.machine.max_k(*node, &key)
                    && k.0 <= max_k
                {
                    return Err(TxError::Validation {
                        rule: format!("R12: k {} not above current max {max_k}", k.0),
                    });
                }
                self.machine
                    .set_cell(*node, &key, *k, value.clone(), fact.txn_id.into())
                    .map_err(gate_err)?;
                Ok(())
            }
            FactPayload::CreateNode { .. } => {
                // Structure facts replay through the arrays path in the
                // host; the gate validates parent liveness here.
                if let Some(parent) = fact.domain.iter().find(|n| **n != NodeId(0)) {
                    self.check_target_alive(*parent)?;
                }
                Ok(())
            }
            FactPayload::DeleteNode { node, .. } => {
                self.check_target_alive(*node)?;
                Ok(())
            }
        }
    }
}

impl MachineGate {
    fn check_target_alive(&self, node: NodeId) -> Result<(), TxError> {
        if !self.machine.arrays.is_alive(node) {
            return Err(TxError::Validation {
                rule: format!("node {} not alive", node.0),
            });
        }
        Ok(())
    }

    fn check_monotone(
        &self,
        node: NodeId,
        key: &Key,
        k: KeyIndex,
        value: &Value,
    ) -> Result<(), TxError> {
        let Some(&mono) = self.monotone.get(key) else {
            return Ok(());
        };
        let Some(latest) = self.machine.latest_cell(node, key) else {
            return Ok(());
        };
        // R7 bites only at the top of the axis (overwrites / appends at
        // the current max); historical k values are untouched.
        if k.0 != self.machine.max_k(node, key).unwrap_or(u64::MAX) {
            return Ok(());
        }
        let ok = match (mono, &latest.value, value) {
            (Monotonicity::Increasing, Value::I64(old), Value::I64(new)) => new >= old,
            (Monotonicity::Decreasing, Value::I64(old), Value::I64(new)) => new <= old,
            (Monotonicity::Increasing, Value::F64(old), Value::F64(new)) => new >= old,
            (Monotonicity::Decreasing, Value::F64(old), Value::F64(new)) => new <= old,
            _ => true,
        };
        if !ok {
            return Err(TxError::Validation {
                rule: format!("R7: monotone attr {} violated", key.as_str()),
            });
        }
        Ok(())
    }
}

fn gate_err(e: lattice_engine::dispatch::DispatchError) -> TxError {
    TxError::Validation {
        rule: e.to_string(),
    }
}
