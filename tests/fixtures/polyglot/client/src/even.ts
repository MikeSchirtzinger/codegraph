/**
 * Case d (D4): cross-file call cycle, same shape as
 * tests/fixtures/typescript/src/even.ts (R5, bare named-import call).
 */
import { isOdd } from './odd';

export function isEven(n: number): boolean {
  return n === 0 ? true : isOdd(n - 1);
}
