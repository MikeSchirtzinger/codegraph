// Package alpha is one of two unrelated packages that export a function
// named Helper. See beta/beta.go and gamma.go (case c, D3).
package alpha

func Helper() {
	println("alpha helper")
}
