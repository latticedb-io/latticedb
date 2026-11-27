//! LatticeDB engine: FSM registry (finalizing/endless, ranks), transition/action
//! dispatch, dependency algebra (AND OR KOFN GUARD INHIBIT XOR MIRROR WEIGHTED),
//! worklist propagation, timer wheel (the only clock — P8), stall detector.

pub mod fsm;
