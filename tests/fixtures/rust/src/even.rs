//! Case d (D4): cross-file call cycle via the classic `is_even`/`is_odd`
//! mutual recursion, split across two modules. The `use` import (rather than
//! a `crate::odd::...` absolute path) keeps the call text an exact match for
//! `odd`'s qualified name, so this resolves via R1 like `main.rs`'s case b.
use crate::odd;

pub fn is_even(n: u32) -> bool {
    if n == 0 {
        true
    } else {
        odd::is_odd(n - 1)
    }
}
