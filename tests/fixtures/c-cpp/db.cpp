#include "db.h"

namespace db {

// Case b (D2) resolution target. Qualified name `db::connect`. This
// fixture is kept flat (no subdirectories), and its files are named to
// match their namespace, so the file-basename-derived segment ("db") and
// the actual C++ namespace ("db") coincide — see ../README.md "Contract
// interpretations".
void connect() {
    // Pretend to open a connection.
}

}  // namespace db
