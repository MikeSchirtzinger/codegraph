// Fixture: Go resolver-cascade cases (see expected.yaml).
package main

import (
	"fmt"
	"os"

	"fixture-go/db"
	"fixture-go/evenodd"
	"fixture-go/handler"
)

func LogStartup() {
	fmt.Println("starting up")
}

// Run exercises three cases:
//   - a (D1): bare same-file call to LogStartup (R3).
//   - b (D2): package-qualified cross-file call. Go calls are always
//     package-qualified, so this exact-matches Connect's qualified name (R1).
//   - e: call to a stdlib symbol never defined in this project ->
//     UNRESOLVED (R6).
func Run() {
	LogStartup()
	db.Connect()
	os.Getenv("PATH")
}

func main() {
	Run()
	Dispatch()
	fmt.Println("IsEven(4) =", evenodd.IsEven(4))
	h := &handler.Handler{}
	h.Serve()
}
