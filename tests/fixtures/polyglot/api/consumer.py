"""Case c (polyglot-specific, D3-adjacent): a bare call to `connect`,
expected to resolve via R5 (project-unique bare name WITHIN PYTHON).

worker/main.go separately defines a function also named `connect`, in a
different language. A resolver that doesn't scope candidates by language
would see two (name="connect", to_type="function") nodes project-wide and
incorrectly fall through to R6/AMBIGUOUS. No call syntax in any of these 6
languages can ever cross a language boundary, so candidates must be scoped
to the calling edge's own language -- see ../README.md "Contract
interpretations". This is the one thing a single-language fixture cannot
test, and the reason this polyglot fixture exists.
"""
from app import connect


def run():
    connect()
