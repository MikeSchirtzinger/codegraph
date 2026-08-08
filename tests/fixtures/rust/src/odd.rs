//! Other half of the case d (D4) cross-file cycle. See `even.rs`.
use crate::even;

pub fn is_odd(n: u32) -> bool {
    if n == 0 {
        false
    } else {
        even::is_even(n - 1)
    }
}
