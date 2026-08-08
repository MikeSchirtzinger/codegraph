/**
 * Case c (D3): project-wide name collision.
 *
 * This is script-mode too (no import/export), sharing the same global scope
 * as alpha.ts/beta.ts at runtime — plain scripts have no file scoping at
 * all, which is the real-world hazard ES modules exist to prevent, and the
 * most realistic way an unqualified TS/JS collision like D3 arises in code
 * that actually runs. (An ES-module `import { helper }` from two
 * differently-named files would need an alias and simply couldn't collide
 * this way — see ../README.md "Contract interpretations".) codegraph's
 * resolver, which does no scope/usage analysis, must surface this as
 * AMBIGUOUS rather than blending or guessing (D3's exact failure mode).
 */
function dispatch(): void {
  helper();
}
