//! LatticeDB pooler: first-party sidecar — identity translation (mTLS/SPIFFE →
//! kernel subject + scopes), churn absorption, credential mapping, short-lived
//! token issuance (phase v1.x). Thin, stateless besides credentials
//! (ARCHITECTURE §2). Shares the wire runtime family (SPEC-02 §12.8).
