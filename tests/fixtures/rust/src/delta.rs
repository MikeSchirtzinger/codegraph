//! Bonus cascade cases that need a named (non-glob) `use` import, kept
//! separate from `gamma.rs`'s glob-import ambiguity case so the two importing
//! styles don't interact.
use crate::alpha::helper;
use crate::db::connection;

/// Bonus (R4, named-import variant): `helper` exists in both `alpha` and
/// `beta` (see case c in `gamma.rs`), but this file's `use` import names
/// `alpha::helper` specifically, so R4 restricts the candidate set to one
/// before R5/R6 are ever reached.
pub fn call_helper_via_import() {
    helper();
}

/// Bonus (R2): `connection` is imported as a *module*, so the call text is
/// the two-segment suffix `connection::connect`, not the full three-segment
/// `db::connection::connect` (that exact form is exercised by case b in
/// `main.rs`, which resolves via R1 instead).
pub fn call_connect_via_module_import() {
    connection::connect();
}
