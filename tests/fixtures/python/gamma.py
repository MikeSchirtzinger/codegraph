"""Case c (D3): project-wide name collision.

Both wildcard imports bring a `helper` into this module's namespace; Python
resolves this by simply letting the second import shadow the first at
runtime (no error, no warning) -- D3's "blended" failure mode, this time
baked into the language itself rather than just codegraph's naive name
lookup. codegraph's resolver has no import-order/shadowing analysis, so it
must report this call as AMBIGUOUS rather than guessing which `helper`
really executes.
"""
from alpha import *
from beta import *


def dispatch():
    helper()
