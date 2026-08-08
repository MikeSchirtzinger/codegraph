/// Fixture: C/C++ resolver-cascade cases (see expected.yaml).
///
/// Kept flat (no subdirectories) so a "module" segment in qualified_name
/// can only ever come from a file's own basename, never from directory
/// structure — see ../README.md "Contract interpretations" for why C/C++
/// uses a different convention here than Go/Java's directory-only rule.
#include <cstdlib>
#include <iostream>

#include "db.h"
#include "even.h"
#include "gamma.h"
#include "legacy.h"

void log_startup() {
    std::cout << "starting up" << std::endl;
}

/// Case a (D1): bare same-file call to log_startup (R3).
/// Case b (D2): namespace-qualified cross-file call (R1).
/// Case e: call to a libc symbol never defined in this project ->
/// UNRESOLVED (R6).
void run() {
    log_startup();
    db::connect();
    std::getenv("PATH");
}

int main() {
    run();
    dispatch();
    legacy_helper(); // bonus (R5): bare, cross-file, no qualification needed
    std::cout << "is_even(4) = " << even::is_even(4) << std::endl;
    return 0;
}
