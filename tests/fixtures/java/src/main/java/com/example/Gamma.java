package com.example;

import static com.example.alpha.Alpha.helper;
import static com.example.beta.Beta.helper;

/**
 * Case c (D3): project-wide name collision.
 *
 * Both static imports bring a `helper` method into this file's unqualified
 * scope. javac would reject this outright ("reference to helper is
 * ambiguous") — but it is syntactically valid Java, and codegraph's
 * resolver, which does no compile-time static-import-collision checking,
 * must independently report AMBIGUOUS rather than blending (D3's exact
 * failure mode) or silently picking one.
 */
public class Gamma {
    public static void dispatch() {
        helper();
    }
}
