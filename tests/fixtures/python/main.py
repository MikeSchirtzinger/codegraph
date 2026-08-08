"""Fixture: Python resolver-cascade cases (see expected.yaml)."""
import os

import db.connection


def log_startup():
    print("starting up")


def run():
    """
    Case a (D1): bare same-file call to log_startup (R3).
    Case b (D2): db.connection is imported by dotted path and called fully
    qualified, so the call text exactly matches the qualified name (R1).
    Case e: call to a stdlib symbol never defined in this project ->
    UNRESOLVED (R6).
    """
    log_startup()
    db.connection.connect()
    os.getenv("PATH")


if __name__ == "__main__":
    run()
