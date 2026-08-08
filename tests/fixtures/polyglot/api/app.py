"""Polyglot fixture: Python service (see ../README.md and ../expected.yaml).

`connect` is intentionally the same bare name as worker/main.go's `connect`
-- see consumer.py and ../README.md "Contract interpretations".
"""


def connect():
    print("python connect")
