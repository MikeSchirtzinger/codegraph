package main

// Case c (D3): project-wide name collision.
//
// Dot-imports bring both alpha.Helper and beta.Helper into this file's
// unqualified scope — the only Go import form that makes a bare,
// syntactically-ambiguous cross-package call possible at all (ordinary Go
// calls are always package-qualified and therefore never ambiguous; see
// ../README.md "Contract interpretations"). `go build` would reject this
// outright ("Helper redeclared in this block"), but it is syntactically
// valid Go, and codegraph's resolver — which does no compile-time
// redeclaration checking — must independently report AMBIGUOUS rather than
// blending (D3's exact failure mode) or silently picking one.
import (
	. "fixture-go/alpha"
	. "fixture-go/beta"
)

func Dispatch() {
	Helper()
}
