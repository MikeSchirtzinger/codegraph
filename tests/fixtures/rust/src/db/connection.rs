//! Case b (D2) resolution target: a function reached only through a
//! module-qualified path from other files. Qualified name `db::connection::connect`
//! (file path `src/db/connection.rs`, with the crate-root `src/` segment
//! elided per the Rust convention documented in `../../README.md`).

pub fn connect() {
    // Pretend to open a connection.
}
