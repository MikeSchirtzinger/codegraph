package com.example.evenodd;

import static com.example.evenodd.Odd.isOdd;

/**
 * Case d (D4): cross-file call cycle (mutual recursion) with Odd.java. The
 * static import makes the call site a bare identifier, so this resolves via
 * R5 (project-unique bare name), not R1/R2 — same reasoning as the
 * TypeScript/Python fixtures' even/odd cycle (R4 only consults import facts
 * for Rust in v1 — see ../../../../../../README.md).
 */
public class Even {
    public static boolean isEven(int n) {
        if (n == 0) {
            return true;
        }
        return isOdd(n - 1);
    }
}
