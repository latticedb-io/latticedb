//! LatticeDB admin plane: gRPC control plane — health, metrics (Prometheus
//! exposition), `plan_schema_txn` dry-run, storage ops, audit export
//! (SPEC-02 §11). Owns the tonic/tokio stack for its listener (SPEC-02 §12.8).
