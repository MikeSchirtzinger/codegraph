//! Fixture: rename-refactor kill-test (see ../expected.yaml and
//! ../../expected.yaml).
mod stale_caller;
mod target;
mod updated_caller;

fn main() {
    stale_caller::use_stale();
    updated_caller::use_updated();
}
