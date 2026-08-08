/**
 * Case d (D4): cross-file call cycle (mutual recursion) with odd.ts. The
 * named import (rather than a namespace import) makes the call site a bare
 * identifier, so this resolves via R5 (project-unique bare name), not R1 —
 * see ../README.md "Contract interpretations" (R4 only consults import
 * facts for Rust in v1, so a bare name reached through any other language's
 * import falls through to R5 whenever it happens to be project-unique).
 */
import { isOdd } from './odd';

export function isEven(n: number): boolean {
  return n === 0 ? true : isOdd(n - 1);
}
