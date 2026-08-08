#!/usr/bin/env python3
"""Index+resolve throughput, and where *that* wall time goes.

Times a full `codegraph index --force` run (discover -> incremental-diff ->
parse/extract -> store -> R1 resolve -> file_ref derivation -- everything
`index::index_project` does in one call) against two corpora:

  - this repo's own src/           (28 files)
  - the whole tests/fixtures/ tree (70 files, all 6 languages + polyglot +
    rename-refactor)

reporting files/nodes/edges per second straight from the CLI's own stdout
summary (regexes below, no separate instrumentation). Also reports the
wall/CPU split (RUSAGE_CHILDREN) for the whole `index` invocation, and a
three-way phase split (connect / schema-init / indexing-body) using the
same stderr-line-watching technique as store_open_cost.py.

Finding this script exists to surface: `store_open_cost.py` shows today's
`codegraph query` does NOT reproduce D6's 37s claim. This script shows
`codegraph index` -- a different operation -- has essentially the same
symptom (high wall time, low CPU) that D6 originally described, just
relocated. `index::store_parsed_file` (src/index/mod.rs) issues one
UNBATCHED, individually-awaited `CREATE` statement per node and per edge --
unlike `index::resolve::write_updates` (src/index/resolve.rs), which
explicitly batches its UPDATEs in chunks of `WRITE_CHUNK_SIZE = 250` and
says why ("one already-open db session throughout ... batched rather than
one query per edge"). The ms/record column below is the evidence: it's
roughly constant across corpus sizes, consistent with a per-round-trip
fixed cost dominating rather than e.g. schema/store-open cost (which would
be a one-time cost, not one that scales with record count).

A standalone `codegraph resolve` re-run is also timed, WITH A CAVEAT
established by reading src/index/resolve.rs directly: its `to_id = ''`
idempotency filter means a rerun only ever reconsiders the
AMBIGUOUS/UNRESOLVED remainder (RESOLVED edges already carry a real to_id
and are excluded by that same WHERE clause) -- never the full edge set
again. So this number is NOT a fresh full-project resolve timing; it's
presented as exactly what it is.

Repro:
  cargo build --release
  python3 scripts/bench/index_throughput.py
"""
import argparse
import os
import re
import resource
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def repo_root() -> Path:
    try:
        out = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True)
        return Path(out.stdout.strip())
    except Exception:
        return Path(__file__).resolve().parents[2]


def find_binary(root: Path) -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(root / "target")))
    return target / "release" / "codegraph"


def ensure_binary(root: Path) -> Path:
    b = find_binary(root)
    if not b.exists():
        print(f"[index_throughput] {b} not found -- building (cargo build --release)...", file=sys.stderr)
        subprocess.run(["cargo", "build", "--release"], cwd=root, check=True)
    if not b.exists():
        sys.exit(f"cargo build finished but {b} still missing -- check CARGO_TARGET_DIR")
    return b


# --- regexes against main.rs's exact println! text (src/main.rs) ---
RE_FILES = re.compile(r"Files:\s+(\d+) scanned, (\d+) indexed, (\d+) unchanged, (\d+) skipped")
RE_NODES = re.compile(r"Nodes:\s+(\d+)")
RE_EDGES_CREATED = re.compile(r"^\s*Edges:\s+(\d+)\s*$", re.MULTILINE)  # index block only (no trailing text)
RE_EDGES_CONSIDERED = re.compile(r"Edges:\s+(\d+)\s+(?:name-edges )?considered")  # both index + resolve blocks
RE_RESOLVED = re.compile(r"Resolved:\s+(\d+) \(")
RE_AMBIGUOUS = re.compile(r"Ambiguous:\s+(\d+) \(")
RE_UNRESOLVED = re.compile(r"Unresolved:\s+(\d+) \(")
RE_FILE_REFS = re.compile(r"File refs:\s+(\d+)")


def run_with_phases(cmd, env, watch_lines=("Connected to SurrealDB", "Schema initialized")):
    """Run a CLI command, splitting wall time at each watch_line's first
    appearance on stderr (in order), plus wall/cpu via RUSAGE_CHILDREN."""
    ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    marks = {}
    remaining = list(watch_lines)
    for line in proc.stderr:
        if remaining and remaining[0] in line:
            marks[remaining[0]] = time.time()
            remaining.pop(0)
    stdout_data = proc.stdout.read()
    proc.wait()
    t1 = time.time()
    ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)

    bounds = [t0] + [marks[w] for w in watch_lines if w in marks] + [t1]
    phases = [bounds[i + 1] - bounds[i] for i in range(len(bounds) - 1)]
    return dict(wall=t1 - t0, cpu=cpu, phases=phases, stdout=stdout_data, rc=proc.returncode)


def parse_index_stats(stdout: str) -> dict:
    def grab(rx, cast=int, group=1):
        m = rx.search(stdout)
        return cast(m.group(group)) if m else None

    return dict(
        files_indexed=grab(RE_FILES, group=2),
        nodes=grab(RE_NODES),
        edges=grab(RE_EDGES_CREATED),
        edges_considered=grab(RE_EDGES_CONSIDERED),
        resolved=grab(RE_RESOLVED),
        ambiguous=grab(RE_AMBIGUOUS),
        unresolved=grab(RE_UNRESOLVED),
        file_refs=grab(RE_FILE_REFS),
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    args = ap.parse_args()

    print("Repro: python3 scripts/bench/index_throughput.py")
    root = repo_root()
    binary = ensure_binary(root)
    env = dict(os.environ)
    env["RUST_LOG"] = "codegraph=info"

    tmp = Path(tempfile.mkdtemp(prefix="codegraph-bench-throughput-"))
    print(f"binary:      {binary}")
    print(f"scratch dir: {tmp}\n")

    corpora = [
        ("self-src", root / "src", 28),
        ("fixture-tree", root / "tests" / "fixtures", 70),
    ]

    for label, path, expected_files in corpora:
        pid = f"bench-idx-{label}"
        db_dir = tmp / f"db-{label}"
        db_url = f"surrealkv://{db_dir}/graph.db"

        index_cmd = [str(binary), "index", str(path), "--project-id", pid, "--db-url", db_url, "--force"]
        r = run_with_phases(index_cmd, env)
        if r["rc"] != 0:
            print(f"[{label}] INDEX FAILED (rc={r['rc']})")
            continue
        stats = parse_index_stats(r["stdout"])

        print(f"=== {label}: {path} ({expected_files} files expected) ===")
        print(f"  wall={r['wall']:.3f}s  cpu={r['cpu']:.3f}s  (wall/cpu ratio: {r['wall']/max(r['cpu'], 1e-6):.1f}x)")
        if len(r["phases"]) == 3:
            print(f"  phases: connect={r['phases'][0]*1000:.1f}ms  "
                  f"schema-init={r['phases'][1]*1000:.1f}ms  "
                  f"index-body(parse+store+resolve)={r['phases'][2]*1000:.1f}ms")
        print(f"  files_indexed={stats['files_indexed']}  nodes={stats['nodes']}  edges={stats['edges']}")
        print(f"  resolution: {stats['edges_considered']} name-edges considered, "
              f"{stats['resolved']} resolved, {stats['ambiguous']} ambiguous, "
              f"{stats['unresolved']} unresolved, {stats['file_refs']} file_refs derived")

        if stats["files_indexed"] and stats["nodes"] is not None and stats["edges"] is not None:
            records = stats["nodes"] + stats["edges"]
            print(f"  -> {stats['files_indexed']/r['wall']:.2f} files/sec, "
                  f"{stats['nodes']/r['wall']:.0f} nodes/sec, {stats['edges']/r['wall']:.0f} edges/sec "
                  f"(full pipeline: parse+extract+store+R1-resolve)")
            print(f"  -> {r['wall']*1000/records:.2f} ms/record ({records} records = nodes+edges) "
                  f"-- compare across corpora: near-constant ms/record is the signature of an "
                  f"unbatched per-record round-trip cost, not a size-scaling algorithm")

        # Standalone resolve re-run -- see module docstring for the
        # to_id='' idempotency caveat: this reconsiders only the leftover
        # AMBIGUOUS/UNRESOLVED subset, not the full edge set again.
        resolve_cmd = [str(binary), "resolve", "--project-id", pid, "--db-url", db_url]
        rr = run_with_phases(resolve_cmd, env)
        if rr["rc"] == 0:
            rstats = parse_index_stats(rr["stdout"])
            considered = rstats["edges_considered"]
            print(f"  standalone `resolve` rerun: wall={rr['wall']:.3f}s cpu={rr['cpu']:.3f}s "
                  f"(ratio {rr['wall']/max(rr['cpu'], 1e-6):.1f}x) for {considered} leftover "
                  f"AMBIGUOUS/UNRESOLVED edges"
                  + (f" -> {considered/rr['wall']:.0f} edges/sec" if considered else ""))
            print("    CAVEAT: NOT a fresh full-project resolve -- src/index/resolve.rs's `to_id = ''`")
            print("    WHERE clause means RESOLVED edges (real to_id) are excluded from any rerun by")
            print("    design (that's what makes reruns incremental-safe); only the still-unresolved")
            print("    remainder is reconsidered. This revises, and does not merely confirm, the")
            print("    '~2,500 edges <1s' figure -- see README.md's Performance section.")
        print()

    shutil.rmtree(tmp, ignore_errors=True)
    print("scratch stores cleaned up")


if __name__ == "__main__":
    main()
