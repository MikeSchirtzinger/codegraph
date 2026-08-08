//! Fixture: Rust resolver-cascade cases (see `../expected.yaml` for the
//! ground truth this project encodes).
//!
//! `main.rs` is the crate root, so its own items sit at the crate root with
//! no module-path prefix (`run`, not `main::run`) — the one Rust-specific
//! wrinkle in an otherwise mechanical file-path-to-module-path mapping; see
//! `../README.md`'s "Contract interpretations".

mod alpha;
mod beta;
mod db;
mod delta;
mod even;
mod gamma;
mod odd;

fn log_startup() {
    println!("starting up");
}

/// Case a (D1): bare same-file call to `log_startup` (R3).
/// Case b (D2): module-qualified cross-file call, spelled out in full from
/// the crate root, so it exact-matches `connect`'s qualified name (R1).
/// Case e: call to an external/stdlib symbol — never defined anywhere in
/// this project, so it terminates R6 as UNRESOLVED rather than an error.
fn run() {
    log_startup();
    db::connection::connect();
    let _args: Vec<String> = std::env::args().collect();
}

fn main() {
    run();
    gamma::dispatch();
    gamma::dispatch_unique();
    delta::call_helper_via_import();
    delta::call_connect_via_module_import();
    println!("is_even(4) = {}", even::is_even(4));
}
