#include "gamma.h"

#include "alpha.h"
#include "beta.h"

using namespace alpha;
using namespace beta;

// Case c (D3): project-wide name collision.
//
// Both using-directives make `helper` visible for unqualified lookup; a
// real compiler only errors at the point of actual ambiguous use ("call of
// overloaded 'helper()' is ambiguous"), not at the using-directives above —
// so this is valid to write, and exactly the kind of ambiguity codegraph's
// resolver (no compile-time overload resolution; design principle 4, "not a
// compiler") must independently detect rather than blend (D3's exact
// failure mode) or silently pick one.
void dispatch() {
    helper();
}
