/**
 * Fixture: TypeScript resolver-cascade cases (see ../expected.yaml).
 */
import * as connection from './db/connection';

function logStartup(): void {
  console.log('starting up');
}

/**
 * Case a (D1): bare same-file call to logStartup (R3).
 * Case b (D2): `db/connection` is imported under a namespace alias, so the
 * call text is the two-segment suffix `connection.connect`, not the full
 * three-segment `src.db.connection.connect` (R2).
 * Case e: call to a built-in global never defined in this project ->
 * UNRESOLVED (R6).
 */
function run(): void {
  logStartup();
  connection.connect();
  JSON.stringify({ ready: true });
}

run();
