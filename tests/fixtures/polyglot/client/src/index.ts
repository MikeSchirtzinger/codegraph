/**
 * Polyglot fixture: TypeScript client (see ../../README.md and
 * ../../expected.yaml).
 */
import * as format from './format';

function logStartup(): void {
  console.log('client starting');
}

/**
 * Case a-equivalent (D1): bare same-file call to logStartup (R3).
 * Case b (D2): namespace-imported cross-file call, suffix match (R2), same
 * shape as tests/fixtures/typescript/src/main.ts.
 * Case e: call to a built-in global never defined in this project ->
 * UNRESOLVED (R6).
 */
function run(): void {
  logStartup();
  format.toJson({ ready: true });
  Math.max(1, 2);
}

run();
