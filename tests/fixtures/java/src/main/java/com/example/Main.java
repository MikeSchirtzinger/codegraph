package com.example;

import com.example.evenodd.Even;

/** Fixture: Java resolver-cascade cases (see ../../../../../expected.yaml). */
public class Main {
    static void logStartup() {
        System.out.println("starting up");
    }

    /**
     * Case a (D1): bare same-file call to logStartup (R3).
     * Case b (D2): fully-qualified, unimported call — Db is referenced by
     * its full package path rather than through an import, so the call
     * text exactly matches the qualified name (R1). This assumes java.rs is
     * extended to capture a method_invocation's `object` field alongside
     * `name` — see ../../../../../README.md "Contract interpretations".
     * Case e: call to a stdlib method never defined in this project ->
     * UNRESOLVED (R6).
     */
    static void run() {
        logStartup();
        com.example.db.Db.connect();
        System.getenv("PATH");
    }

    public static void main(String[] args) {
        run();
        Gamma.dispatch();
        System.out.println("isEven(4) = " + Even.isEven(4));
    }
}
