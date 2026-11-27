//! The FSM registry (SPEC-01 §8, T-201): runtime state machines over the
//! compiled models.
//!
//! Laws enforced here (belt-and-suspenders over the schema-level guarantees
//! the compiler already proved at commit):
//! - **Declared motion only**: a node moves `from → to` iff the model
//!   declares a transition for that pair — there is no other state-write
//!   path (P2: state is derived, never written).
//! - **Backward prohibition is a fixed feature** (R4): in a finalizing
//!   model every accepted motion goes to a strictly greater rank; cycles
//!   are legal ONLY in endless models. No retraction machinery exists.
//! - **Terminals are absorbing**: motion OUT of a finalizing terminal is
//!   rejected — no transition may leave a terminal.
//! - **State codes**: each model's states get dense `u8` codes (the SoA
//!   `state[u8]` representation); assignment is declaration order —
//!   deterministic per epoch.
//!
//! Endless models: no ranks, no terminals; cycles among declared motions
//! are legal (SGT-8.3) and are the *only* way such machines run.

use lattice_schema::compile::{Compiled, CompiledModel};
use lattice_schema::entities::ModelClass;
use lattice_schema::key::Key;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    UnknownModel {
        model: Key,
    },
    UnknownState {
        model: Key,
        state: Key,
    },
    /// `from → to` is not a declared transition (P2: declared motion only).
    UndeclaredMotion {
        model: Key,
        from: Key,
        to: Key,
    },
    /// Backward motion in a finalizing model (R4 — structurally impossible;
    /// this runtime check guards the registry itself).
    BackwardRank {
        model: Key,
        from: Key,
        to: Key,
        from_rank: u64,
        to_rank: u64,
    },
    /// Motion out of a finalizing terminal — terminals are absorbing.
    TerminalState {
        model: Key,
        state: Key,
    },
    /// Structural-array failure inside effects (spawn/cancel).
    Arrays(lattice_core::arrays::ArrayError),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::UnknownModel { model } => write!(f, "unknown state model `{model}`"),
            EngineError::UnknownState { model, state } => {
                write!(f, "model `{model}` has no state `{state}`")
            }
            EngineError::UndeclaredMotion { model, from, to } => {
                write!(
                    f,
                    "model `{model}`: motion {from}→{to} is not a declared transition"
                )
            }
            EngineError::BackwardRank {
                model,
                from,
                to,
                from_rank,
                to_rank,
            } => write!(
                f,
                "model `{model}`: backward motion {from}→{to} (rank {from_rank} → {to_rank}); use a re-run instance"
            ),
            EngineError::TerminalState { model, state } => {
                write!(
                    f,
                    "model `{model}`: state `{state}` is terminal — terminals are absorbing"
                )
            }
            EngineError::Arrays(e) => write!(f, "arrays: {e}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<lattice_core::arrays::ArrayError> for EngineError {
    fn from(e: lattice_core::arrays::ArrayError) -> Self {
        EngineError::Arrays(e)
    }
}

/// One state's runtime info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateInfo {
    pub key: Key,
    /// Dense u8 code (SoA `state[u8]` representation), declaration order.
    pub code: u8,
    pub success_terminal: bool,
    pub failure_terminal: bool,
    /// Finalizing only.
    pub rank: Option<u64>,
}

/// One model's runtime FSM.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeModel {
    pub class: ModelClass,
    pub initial: StateInfo,
    pub states: BTreeMap<Key, StateInfo>,
    /// Declared motions: (from, to) → transition keys.
    pub motions: BTreeMap<(Key, Key), Vec<Key>>,
}

fn missing_rank(model: &Key, state: &Key) -> ! {
    panic!(
        "registry invariant: finalizing model {model:?} state {state:?} carries no rank — compiler/registry desync"
    );
}

impl RuntimeModel {
    pub fn state_by_code(&self, code: u8) -> Option<&StateInfo> {
        self.states.values().find(|s| s.code == code)
    }

    pub fn is_terminal(&self, state: &Key) -> Option<bool> {
        self.states
            .get(state)
            .map(|s| s.success_terminal || s.failure_terminal)
    }
}

/// All compiled models, keyed by model key.
#[derive(Debug, Clone, Default)]
pub struct FsmRegistry {
    models: BTreeMap<Key, RuntimeModel>,
}

/// The verdict of a validated motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedMotion {
    /// The (single, deterministic) transition key for this pair.
    pub transition: Key,
    pub from: StateInfo,
    pub to: StateInfo,
}

impl FsmRegistry {
    /// Build from the compiled schema epoch.
    pub fn from_compiled(compiled: &Compiled) -> FsmRegistry {
        let mut models = BTreeMap::new();
        for (mkey, model) in &compiled.models {
            models.insert(mkey.clone(), RuntimeModel::from_compiled(mkey, model));
        }
        FsmRegistry { models }
    }

    pub fn model(&self, model: &Key) -> Option<&RuntimeModel> {
        self.models.get(model)
    }

    /// Mutable access — TEST ONLY (forges motions to prove the rank/terminal
    /// gates fire even when a registry bug declares a bad pair).
    #[doc(hidden)]
    pub fn model_mut(&mut self, model: &Key) -> Option<&mut RuntimeModel> {
        self.models.get_mut(model)
    }

    /// Validate a state motion (SPEC-01 §8): declared-only, rank-progression
    /// for finalizing, absorbing terminals. Returns the allowed transition.
    pub fn check_motion(
        &self,
        model: &Key,
        from: &Key,
        to: &Key,
    ) -> Result<AllowedMotion, EngineError> {
        let m = self
            .models
            .get(model)
            .ok_or_else(|| EngineError::UnknownModel {
                model: model.clone(),
            })?;
        let fi = m
            .states
            .get(from)
            .ok_or_else(|| EngineError::UnknownState {
                model: model.clone(),
                state: from.clone(),
            })?;
        let ti = m.states.get(to).ok_or_else(|| EngineError::UnknownState {
            model: model.clone(),
            state: to.clone(),
        })?;

        let keys = m.motions.get(&(from.clone(), to.clone())).ok_or_else(|| {
            EngineError::UndeclaredMotion {
                model: model.clone(),
                from: from.clone(),
                to: to.clone(),
            }
        })?;
        if keys.len() != 1 {
            // Registry invariant: multiple transitions for one (from, to)
            // pair are legal in the schema (different requirements/actors),
            // but motion validation reports the pair as allowed once; the
            // dispatcher (T-202) selects among the keys by requirements.
            // Here we take the lexicographically first key for the verdict —
            // deterministic and rank/terminal checks are pair-level anyway.
        }
        let Some(transition) = keys.iter().min().cloned() else {
            return Err(EngineError::UndeclaredMotion {
                model: model.clone(),
                from: from.clone(),
                to: to.clone(),
            });
        };

        if m.class == ModelClass::Finalizing {
            if fi.success_terminal || fi.failure_terminal {
                return Err(EngineError::TerminalState {
                    model: model.clone(),
                    state: from.clone(),
                });
            }
            // Registry invariant: finalizing states always carry ranks
            // (compiler-guaranteed). A violation is a loud panic, never a
            // silent pass.
            let fr = fi.rank.unwrap_or_else(|| missing_rank(model, from));
            let tr = ti.rank.unwrap_or_else(|| missing_rank(model, to));
            if tr <= fr {
                return Err(EngineError::BackwardRank {
                    model: model.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    from_rank: fr,
                    to_rank: tr,
                });
            }
        }
        Ok(AllowedMotion {
            transition,
            from: fi.clone(),
            to: ti.clone(),
        })
    }
}

impl RuntimeModel {
    fn from_compiled(_key: &Key, model: &CompiledModel) -> RuntimeModel {
        let mut states = BTreeMap::new();
        for (i, (skey, success, failure)) in model.states.iter().enumerate() {
            let rank = model.ranks.get(skey).copied();
            states.insert(
                skey.clone(),
                StateInfo {
                    key: skey.clone(),
                    code: i as u8,
                    success_terminal: *success,
                    failure_terminal: *failure,
                    rank,
                },
            );
        }
        let Some(initial) = states.get(&model.initial).cloned() else {
            // Compiler-guaranteed; a violation is a loud registry-construction
            // bug, never a silent default.
            panic!("registry invariant: initial state missing after compile");
        };
        RuntimeModel {
            class: model.class,
            initial,
            states,
            motions: model.motions.clone(),
        }
    }
}
