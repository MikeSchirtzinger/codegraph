/**
 * Script-mode file: no top-level import/export, so this is global scope,
 * not an ES module. See gamma.ts for why that's the realistic vehicle for a
 * same-name collision in TypeScript/JavaScript (case c, D3).
 */
function helper(): void {
  console.log('alpha helper');
}
