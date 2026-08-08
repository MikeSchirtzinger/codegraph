//! Also calls `target::helper`, but -- unlike stale_caller.rs -- this file
//! IS correctly updated in after/ (see that version there). Included so
//! the kill-test proves the tool distinguishes the one broken caller from
//! a properly migrated one, rather than blanket-flagging everything that
//! merely used to mention the old name.
use crate::target;

pub fn use_updated() {
    target::helper();
}
