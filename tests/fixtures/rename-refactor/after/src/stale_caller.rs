//! Calls `target::helper`. This file is BYTE-IDENTICAL in before/ and
//! after/ -- simulating the caller a real refactor misses. See
//! ../expected.yaml and ../../expected.yaml: the same call site is
//! RESOLVED in before/ and UNRESOLVED in after/, which is exactly what a
//! real `impact`/`deps` query MUST flag (the kill-test from the spec: "the
//! exact scenario a skeptical engineer will try in 2 hours").
use crate::target;

pub fn use_stale() {
    target::helper();
}
