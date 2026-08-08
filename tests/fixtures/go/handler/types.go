// Package handler holds case f: a cross-file receiver-method (member_of)
// case. Handler is defined here; its method Serve lives in serve.go, a
// DIFFERENT file within this same package.
package handler

type Handler struct {
	addr string
}
