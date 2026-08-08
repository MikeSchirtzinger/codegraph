//! Correctly migrated to call target::helper_v2 -- contrast with
//! stale_caller.rs, which was NOT updated to match the rename.
use crate::target;

pub fn use_updated() {
    target::helper_v2();
}
