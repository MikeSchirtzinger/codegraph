#include "legacy.h"

#include <stdio.h>
#include <stdlib.h>

/* Bonus (R5): a plain C function, unique project-wide, called bare with no
 * qualification at all from a DIFFERENT file (main.cpp) with no import-like
 * mechanism in between (just the shared declaration in legacy.h). C has no
 * namespace/module syntax, so a bare cross-file call genuinely doesn't need
 * same-file scope or an import to be valid, unlike every other language in
 * this suite — see ../README.md "Contract interpretations" (R5 is most
 * naturally demonstrated in C among these six languages). Deliberately not
 * called from within this same file — that would resolve via R3 instead
 * and not demonstrate R5 at all. */
void legacy_helper(void) {
    printf("legacy helper\n");
}
