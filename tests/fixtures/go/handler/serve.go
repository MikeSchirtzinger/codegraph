package handler

// Serve's receiver type Handler is defined in a DIFFERENT file (types.go)
// within this same package. This is case f's resolution target: Serve ->
// Handler is a member_of edge (not a calls edge), and -- like the evenodd
// cross-file cycle case -- resolves via R5 (project-unique bare name)
// rather than R3, since v1's cascade has no same-package tier and R3 is
// file-scoped only. See ../README.md "Contract interpretations".
func (h *Handler) Serve() {
	println("serving", h.addr)
}
