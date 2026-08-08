//! The function that gets renamed between before/ and after/. This is the
//! "half-finished refactor": the definition was renamed here, but (as real
//! refactors sometimes go) not every caller was updated to match -- see
//! stale_caller.rs vs updated_caller.rs, and ../expected.yaml.
pub fn helper_v2() {
    println!("helper_v2");
}
