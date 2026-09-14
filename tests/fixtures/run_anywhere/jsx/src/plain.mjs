// A .mjs module, deliberately plain JavaScript with no JSX. It stays on the
// TypeScript grammar and must keep parsing.
export function loadRows(source) {
  return source.map((r) => ({ id: r.id, label: r.name }));
}
