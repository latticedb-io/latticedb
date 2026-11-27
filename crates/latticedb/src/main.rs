//! LatticeDB Host binary: assembly + config (ARCHITECTURE §3). One process —
//! owns all listeners; kernel crates run on dedicated OS thread pools (ARCH §8
//! execution model).
fn main() {
    println!("latticedb {}", env!("CARGO_PKG_VERSION"));
}
