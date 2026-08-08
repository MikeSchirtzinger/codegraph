// Package evenodd holds the case d (D4) cross-file call cycle.
package evenodd

// IsEven and IsOdd (odd.go) mutually recurse across files within this one
// package. No import is needed between them — files in the same Go package
// share scope implicitly (unlike a cross-package cycle, which `go build`
// would reject outright as an import cycle; that's why this cycle is
// same-package/cross-file rather than cross-package like case b).
//
// Consequence for the resolver: the bare call to IsOdd below is a different
// FILE, not a different PACKAGE, from IsOdd's perspective, so it resolves
// via R5 (project-unique bare name), not R3 (same-file). v1's cascade has
// no same-package tier — see ../README.md "Contract interpretations" for
// why that's a documented precision gap, not a bug in this fixture.
func IsEven(n int) bool {
	if n == 0 {
		return true
	}
	return IsOdd(n - 1)
}
