package evenodd

// IsOdd is the other half of the case d (D4) cross-file cycle. See even.go.
func IsOdd(n int) bool {
	if n == 0 {
		return false
	}
	return IsEven(n - 1)
}
