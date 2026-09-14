// A .jsx file whose body is a JSX element. Under the plain TypeScript
// grammar `<div>` parses as a comparison, the function body becomes an
// error node, and none of these functions reach the graph. Under TSX they
// all do.
export function renderPanel(title, rows) {
  return (
    <div className="panel">
      <h2>{title}</h2>
      <ul>
        {rows.map((row) => (
          <li key={row.id}>{row.label}</li>
        ))}
      </ul>
    </div>
  );
}

export function panelTitle(prefix) {
  return prefix + " panel";
}
