//! One of two unrelated modules that define a function named `helper`.
//!
//! Exercises: case c (D3, together with `beta.rs`) — see `gamma.rs` for the
//! ambiguous call site — and the bonus R4 case (together with `delta.rs`,
//! which imports this `helper` specifically).

pub fn helper() {
    println!("alpha helper");
}

/// Unique project-wide (no other module defines this name). Referenced by
/// `gamma.rs` via a glob import to demonstrate that an unambiguous name
/// pulled in alongside an ambiguous one still resolves cleanly.
pub fn unique_alpha_fn() {
    println!("alpha-only");
}
