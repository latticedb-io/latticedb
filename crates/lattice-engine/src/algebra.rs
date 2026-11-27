//! Dependency algebra (SPEC-01 §10, T-203): the 8 fixed edge kinds and
//! their param validation + satisfaction evaluation.
//!
//! Fixed registry — new kinds are new engine versions (P4 reducibility at
//! design review, never in the catalog).
//!
//! Per-kind params (R1-validated):
//! - `AND {}` — satisfied when ALL incoming AND-edges' sources are satisfied.
//! - `OR {}` — at least one.
//! - `KOFN {k: u32}` — at least k of the incoming edges' sources; k ≤ N.
//! - `GUARD {sigma, tau}` — armed when the source reaches state `sigma`;
//!   fires the armed `tau` transition on the target (lag/deadline via the
//!   timer wheel, T-205).
//! - `INHIBIT {activation}` — blocks while the source is (a) not final-
//!   success (`on_source_final`) or (b) active (`while_source_active`).
//! - `XOR {}` — exclusive selection (select_branch machinery, T-202).
//! - `MIRROR {}` — target mirrors the source's state (same state code).
//! - `WEIGHTED {weight}` — decimal(6,4); edges into one target must sum to
//!   exactly 1.0000 (SGT-10.2).
//!
//! All evaluations are monotone and order-insensitive (confluence, P15).

use lattice_core::value::Decimal;
use lattice_schema::key::Key;
use std::collections::BTreeMap;
use std::fmt;

/// Kinds (registry byte order = SPEC-01 §10 order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    And,
    Or,
    Kofn,
    Guard,
    Inhibit,
    Xor,
    Mirror,
    Weighted,
}

impl Kind {
    pub fn byte(self) -> u8 {
        match self {
            Kind::And => 1,
            Kind::Or => 2,
            Kind::Kofn => 3,
            Kind::Guard => 4,
            Kind::Inhibit => 5,
            Kind::Xor => 6,
            Kind::Mirror => 7,
            Kind::Weighted => 8,
        }
    }

    pub fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::And,
            2 => Kind::Or,
            3 => Kind::Kofn,
            4 => Kind::Guard,
            5 => Kind::Inhibit,
            6 => Kind::Xor,
            7 => Kind::Mirror,
            8 => Kind::Weighted,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::And => "AND",
            Kind::Or => "OR",
            Kind::Kofn => "KOFN",
            Kind::Guard => "GUARD",
            Kind::Inhibit => "INHIBIT",
            Kind::Xor => "XOR",
            Kind::Mirror => "MIRROR",
            Kind::Weighted => "WEIGHTED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    OnSourceFinal,
    WhileSourceActive,
}

/// Per-kind params (validated, engine-side resolved forms).
#[derive(Debug, Clone, PartialEq)]
pub enum KindParams {
    And,
    Or,
    Kofn {
        k: u32,
    },
    Guard {
        sigma: Key,
        tau: Key,
        lag_ms: Option<u64>,
        deadline_ms: Option<u64>,
        predicate: Option<Key>,
    },
    Inhibit {
        activation: Activation,
    },
    Xor,
    Mirror,
    Weighted {
        weight: Decimal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    /// R1: parameter out of range for the kind (SGT-10.1).
    InvalidKindParams {
        kind: &'static str,
        reason: String,
    },
    /// SGT-10.2: WEIGHTED edges into one target must sum to exactly 1.0000.
    WeightsNotNormalized {
        target: u32,
        total_unscaled: i128,
    },
    /// SGT-10.3: GUARD `sigma`/`tau` or `predicate` unresolved.
    UnresolvedReference {
        kind: &'static str,
        reference: String,
    },
    UnknownKind(u8),
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamError::InvalidKindParams { kind, reason } => {
                write!(f, "INVALID_KIND_PARAMS: {kind}: {reason}")
            }
            ParamError::WeightsNotNormalized {
                target,
                total_unscaled,
            } => {
                write!(
                    f,
                    "INVALID_KIND_PARAMS: WEIGHTED edges into target {target} sum to {total_unscaled}/10000 — must be exactly 10000 (1.0000)"
                )
            }
            ParamError::UnresolvedReference { kind, reference } => {
                write!(f, "SCHEMA: {kind} reference `{reference}` unresolved")
            }
            ParamError::UnknownKind(b) => write!(f, "unknown edge kind byte {b}"),
        }
    }
}

impl std::error::Error for ParamError {}

/// Validate one KOFN param: k ≥ 1 (SGT-10.1 checks k ≤ N at the graph level —
/// see `validate_kofn_against_degree`).
pub fn validate_kofn(k: u32) -> Result<KindParams, ParamError> {
    if k == 0 {
        return Err(ParamError::InvalidKindParams {
            kind: "KOFN",
            reason: "k must be ≥ 1".into(),
        });
    }
    Ok(KindParams::Kofn { k })
}

/// SGT-10.1: KOFN k over N incoming edges — k must not exceed the number of
/// sources feeding the target.
pub fn validate_kofn_against_degree(k: u32, incoming_edges: usize) -> Result<(), ParamError> {
    if (k as usize) > incoming_edges {
        return Err(ParamError::InvalidKindParams {
            kind: "KOFN",
            reason: format!("k={k} over {incoming_edges} incoming edges"),
        });
    }
    Ok(())
}

/// SGT-10.2: WEIGHTED edges into one target — decimal(6,4) unscaled values
/// must sum to exactly 10000 (= 1.0000). All values must be in (0, 1].
pub fn validate_weights_normalized(
    target: u32,
    unscaled_weights: &[i128],
) -> Result<(), ParamError> {
    for &w in unscaled_weights {
        if w <= 0 || w > 10_000 {
            return Err(ParamError::InvalidKindParams {
                kind: "WEIGHTED",
                reason: format!("weight {w}/10000 out of range (0, 1]"),
            });
        }
    }
    let total: i128 = unscaled_weights.iter().sum();
    if total != 10_000 {
        return Err(ParamError::WeightsNotNormalized {
            target,
            total_unscaled: total,
        });
    }
    Ok(())
}

/// SGT-10.3: GUARD param resolution — `sigma` must exist in the source's
/// model, `tau` in the target's transition set, predicate (if set) in the
/// rule set. Callers pass the resolved membership sets.
pub fn validate_guard_params(
    sigma: &Key,
    tau: &Key,
    predicate: &Option<Key>,
    sigma_states: &BTreeMap<Key, ()>,
    tau_transitions: &BTreeMap<Key, ()>,
    rules: &BTreeMap<Key, ()>,
) -> Result<KindParams, ParamError> {
    if !sigma_states.contains_key(sigma) {
        return Err(ParamError::UnresolvedReference {
            kind: "GUARD",
            reference: format!("sigma={sigma}"),
        });
    }
    if !tau_transitions.contains_key(tau) {
        return Err(ParamError::UnresolvedReference {
            kind: "GUARD",
            reference: format!("tau={tau}"),
        });
    }
    if let Some(p) = predicate {
        if !rules.contains_key(p) {
            return Err(ParamError::UnresolvedReference {
                kind: "GUARD",
                reference: format!("predicate={p}"),
            });
        }
    }
    Ok(KindParams::Guard {
        sigma: sigma.clone(),
        tau: tau.clone(),
        lag_ms: None,
        deadline_ms: None,
        predicate: predicate.clone(),
    })
}

/// One incoming dependency edge: kind + whether its source is satisfied +
/// the edge's normalized weight (unscaled /10000; WEIGHTED only, ignored by
/// other kinds).
pub type IncomingEdge = (Kind, bool, i128);

/// Satisfaction verdict over a target's incoming dependency set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Satisfied,
    NotSatisfied,
    /// No incoming dependency edges: a root target is trivially satisfied.
    Unconditional,
}

/// Evaluate a target's incoming dependency set (SPEC-01 §10).
///
/// Kind groups combine conjunctively when present: all AND edges satisfied,
/// any OR edge satisfied, KOFN satisfied_count ≥ k, WEIGHTED satisfied edges
/// spanning the full normalized weight (sum of satisfied weights = 10000).
/// GUARD/INHIBIT/XOR/MIRROR are not satisfaction kinds (arming / blocking /
/// selection / mirroring) and are evaluated by their own paths.
///
/// `kofn_k`: the KOFN threshold for this target (validated k ≤ degree,
/// SGT-10.1). All KOFN edges of one target share the k.
///
/// Order-insensitive: the verdict depends only on the multiset of edges (P15
/// confluence).
pub fn evaluate_incoming(edges: &[IncomingEdge], kofn_k: Option<u32>) -> Verdict {
    if edges.is_empty() {
        return Verdict::Unconditional;
    }
    let mut and_ok = true;
    let mut or_ok: Option<bool> = None;
    let mut kofn_edges = 0usize;
    let mut kofn_satisfied = 0usize;
    let mut weighted = 0i128;
    let mut weighted_seen = false;
    for &(kind, satisfied, weight) in edges {
        match kind {
            Kind::And => {
                if !satisfied {
                    and_ok = false;
                }
            }
            Kind::Or => {
                *or_ok.get_or_insert(false) |= satisfied;
            }
            Kind::Kofn => {
                kofn_edges += 1;
                if satisfied {
                    kofn_satisfied += 1;
                }
            }
            Kind::Weighted => {
                weighted_seen = true;
                if satisfied {
                    weighted += weight;
                }
            }
            Kind::Guard | Kind::Inhibit | Kind::Xor | Kind::Mirror => {
                // non-satisfaction kinds — excluded from this rollup
            }
        }
    }
    let kofn_ok = match (kofn_edges, kofn_k) {
        (0, _) => true, // no KOFN edges: group absent
        (_, Some(k)) => kofn_satisfied >= k as usize,
        // Absent k param: require all (the conservative default; callers
        // building graphs via validate_kofn always pass k).
        (_, None) => kofn_satisfied == kofn_edges,
    };
    // Satisfied edges deposit their declared weight (unscaled /10000);
    // satisfaction = the satisfied weights sum to exactly the normalized
    // pool 10000 (= 1.0000). Weights are normalized at edge creation
    // (SGT-10.2 validate_weights_normalized).
    let weighted_ok = !weighted_seen || weighted == 10_000;
    if and_ok && or_ok.unwrap_or(true) && kofn_ok && weighted_ok {
        Verdict::Satisfied
    } else {
        Verdict::NotSatisfied
    }
}
