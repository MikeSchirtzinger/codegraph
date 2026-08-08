#include "odd.h"

#include "even.h"

namespace odd {

// Other half of the case d (D4) cross-file cycle. See even.cpp.
bool is_odd(int n) {
    if (n == 0) {
        return false;
    }
    return even::is_even(n - 1);
}

}  // namespace odd
