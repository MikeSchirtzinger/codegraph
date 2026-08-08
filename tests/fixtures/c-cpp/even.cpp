#include "even.h"

#include "odd.h"

namespace even {

// Case d (D4): cross-file call cycle (mutual recursion) with odd.cpp. Each
// side includes only the OTHER's header (not each other's own), so there is
// no header-include cycle, just the ordinary two-header mutual-recursion
// pattern.
bool is_even(int n) {
    if (n == 0) {
        return true;
    }
    return odd::is_odd(n - 1);
}

}  // namespace even
