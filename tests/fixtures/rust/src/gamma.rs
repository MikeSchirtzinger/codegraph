//! Case c (D3): project-wide name collision.
//!
//! Neither `alpha` nor `beta` is *this* file, and nothing here qualifies
//! which `helper` is meant, so a resolver without file/module scoping would
//! blend both hits (D3's exact failure mode, verified on codegraph's own
//! `walk_calls` helper repeated across six extractor files). The glob
//! imports are what make the bare calls valid, compiling Rust in the first
//! place — rustc itself would reject a *truly* unqualified, unimported
//! `helper()` call here (E0425), so this is the realistic way ambiguity
//! like D3 arises in code that actually builds (rustc separately rejects
//! genuinely ambiguous glob names with E0659; codegraph's resolver has no
//! type/usage analysis to tell the two situations apart, so both must
//! surface as AMBIGUOUS rather than silently picking one).
use crate::alpha::*;
use crate::beta::*;

pub fn dispatch() {
    helper();
}

/// Bonus (R4, glob variant): `unique_alpha_fn` is only brought into scope by
/// `alpha`'s glob import (`beta` has no such name), so the import fact
/// narrows the candidate set to exactly one even though this call, like
/// `dispatch`'s above, arrives via a wildcard import rather than a named one.
pub fn dispatch_unique() {
    unique_alpha_fn();
}
