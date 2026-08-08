// Polyglot fixture: Go worker (see ../README.md and ../expected.yaml).
//
// `connect` is intentionally the same bare name as api/app.py's `connect`
// -- see ../README.md "Contract interpretations" and api/consumer.py for
// why that must not cause cross-language ambiguity.
package main

import "fmt"

func connect() {
	fmt.Println("go connect")
}

// Run calls connect from the same file and package -- ordinary R3, no
// cross-language concern at all (same-file resolution doesn't need
// language-scoping; the real cross-language stress test is
// api/consumer.py's R5 lookup).
func Run() {
	connect()
}

func main() {
	Run()
}
