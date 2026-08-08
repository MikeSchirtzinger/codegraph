"""Case d (D4): cross-file call cycle (mutual recursion) with odd.py. The
`from ... import` form makes the call site a bare identifier, so this
resolves via R5 (project-unique bare name) rather than R1/R2 — see
../README.md "Contract interpretations" (same reasoning as the TypeScript
fixture's even.ts/odd.ts)."""
from odd import is_odd


def is_even(n):
    if n == 0:
        return True
    return is_odd(n - 1)
