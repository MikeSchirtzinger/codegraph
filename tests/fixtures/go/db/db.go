// Package db holds the case b (D2) resolution target.
package db

// Connect is reached only through a package-qualified call from other
// files. Qualified name `db::Connect` — Go's "module" unit is the package
// (a directory), not the file, unlike Rust/Python/TypeScript — see
// ../README.md "Contract interpretations".
func Connect() {
	// Pretend to open a connection.
}
