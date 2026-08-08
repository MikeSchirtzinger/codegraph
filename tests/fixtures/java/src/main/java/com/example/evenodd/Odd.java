package com.example.evenodd;

import static com.example.evenodd.Even.isEven;

/** Other half of the case d (D4) cross-file cycle. See Even.java. */
public class Odd {
    public static boolean isOdd(int n) {
        if (n == 0) {
            return false;
        }
        return isEven(n - 1);
    }
}
