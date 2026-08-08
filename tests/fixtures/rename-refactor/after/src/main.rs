//! Fixture: rename-refactor kill-test (see ../expected.yaml and
//! ../../expected.yaml). Byte-identical to before/src/main.rs -- the entry
//! point itself is untouched by the refactor; only target.rs and
//! updated_caller.rs differ between before/ and after/.
mod stale_caller;
mod target;
mod updated_caller;

fn main() {
    stale_caller::use_stale();
    updated_caller::use_updated();
}
