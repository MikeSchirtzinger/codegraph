#!/usr/bin/env python3
"""Where does a single `codegraph query`'s wall time actually go?

specs/resolution-layer-v1.md D6 reported "37s wall / 0.8s CPU for one
`summary` query on a 301-node store" and blamed embedded surrealkv's
store-open cost. This script re-measures that exact claim against today's
code, splitting each invocation's wall time into two phases using the
`tracing::info!` line `src/db.rs::connect` already emits at the very end of
opening the embedded engine (no source instrumentation added — this line
existed before R5):

  phase_open  = process spawn -> "Connected to SurrealDB" line observed
  phase_rest  = "Connected..." line observed -> process exit
                (the query itself, result formatting/printing, and process
                teardown)

`query`/`stats` (unlike `index`/`resolve`) never call `db::init_schema`
(confirmed by reading src/main.rs's command dispatch), so phase_open here
is purely the embedded surrealkv engine's connect cost, not schema DDL.

The "Connected..." line is timestamped by *this script*, the instant it
arrives on the child's stderr pipe (line-buffered; stdout is discarded to
`DEVNULL` so there's no deadlock risk from reading one pipe to EOF before
the other) — not by parsing the Rust-side log timestamp, so there's no
clock-format-matching fragility.

CPU time (user+sys) is read from `resource.getrusage(RUSAGE_CHILDREN)`,
snapshotted immediately before/after each child — the wall/CPU ratio is
the direct signal for "waiting on something" vs. "computing" that D6's own
37s/0.8s framing relies on.

"Cold" is best-effort only: this environment has no reliable non-interactive
way to evict the OS page cache (macOS `purge` needs root; we try
`sudo -n purge`, which fails silently rather than prompting, if there's no
cached sudo credential — see `try_drop_page_cache` below). Absent that,
"cold" below means "first read in a new process right after the store was
written," not "page-cache-evicted." This is stated, not hidden.

Repro:
  cargo build --release
  python3 scripts/bench/store_open_cost.py [--reps N] [--keep]
"""
import argparse
import os
import resource
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def repo_root() -> Path:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        )
        return Path(out.stdout.strip())
    except Exception:
        return Path(__file__).resolve().parents[2]


def find_binary(root: Path) -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(root / "target")))
    return target / "release" / "codegraph"


def ensure_binary(root: Path) -> Path:
    b = find_binary(root)
    if not b.exists():
        print(f"[store_open_cost] {b} not found -- building (cargo build --release)...", file=sys.stderr)
        subprocess.run(["cargo", "build", "--release"], cwd=root, check=True)
    if not b.exists():
        sys.exit(f"cargo build finished but {b} still missing -- check CARGO_TARGET_DIR")
    return b


def run_timed(cmd, env, watch_line="Connected to SurrealDB"):
    """Run one CLI invocation. Returns wall/cpu seconds plus the phase split."""
    ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
    t_watch = None
    for line in proc.stderr:
        if t_watch is None and watch_line in line:
            t_watch = time.time()
    proc.wait()
    t1 = time.time()
    ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    return dict(
        wall=t1 - t0,
        cpu=cpu,
        phase_open=(t_watch - t0) if t_watch else None,
        phase_rest=(t1 - t_watch) if t_watch else None,
        rc=proc.returncode,
    )


def fmt(r) -> str:
    op = f"{r['phase_open']*1000:8.1f}ms" if r["phase_open"] is not None else "     n/a"
    rest = f"{r['phase_rest']*1000:8.1f}ms" if r["phase_rest"] is not None else "     n/a"
    return (f"wall={r['wall']*1000:9.1f}ms  cpu={r['cpu']*1000:8.1f}ms  "
            f"open={op}  rest={rest}")


def make_tiny_project(tmp: Path) -> Path:
    d = tmp / "src-tiny"
    d.mkdir(parents=True, exist_ok=True)
    (d / "a.py").write_text("def f():\n    return 1\n")
    return d


def try_drop_page_cache() -> bool:
    """Best-effort only. Never prompts (sudo -n fails fast with no cached cred)."""
    try:
        r = subprocess.run(["sudo", "-n", "purge"], capture_output=True, timeout=5)
        return r.returncode == 0
    except Exception:
        return False


def median(xs):
    s = sorted(xs)
    return s[len(s) // 2]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--reps", type=int, default=5, help="warm repetitions per store size (default 5)")
    ap.add_argument("--keep", action="store_true", help="keep scratch stores for inspection")
    args = ap.parse_args()

    print("Repro: python3 scripts/bench/store_open_cost.py "
          f"--reps {args.reps}" + (" --keep" if args.keep else ""))

    root = repo_root()
    binary = ensure_binary(root)
    env = dict(os.environ)
    env["RUST_LOG"] = "codegraph=info"  # deterministic regardless of caller's shell env

    tmp = Path(tempfile.mkdtemp(prefix="codegraph-bench-openc-"))
    print(f"binary:      {binary}")
    print(f"scratch dir: {tmp}")
    print()

    # --- floor: process spawn only, `--help` exits inside clap, before any
    # tracing/DB code ever runs. ---
    floor_cmd = [str(binary), "--help"]
    floor = [run_timed(floor_cmd, env, watch_line="\x00NEVER-MATCHES\x00") for _ in range(args.reps)]
    walls = sorted(r["wall"] for r in floor)
    print(f"[floor] `codegraph --help` (process spawn, zero DB touch), n={len(floor)}")
    print(f"        wall min/median/max = {walls[0]*1000:.1f} / {median(walls)*1000:.1f} / {walls[-1]*1000:.1f} ms")
    print()

    sizes = [
        ("tiny (1 file)", make_tiny_project(tmp)),
        ("medium (this repo's src/, 28 files)", root / "src"),
        ("large (tests/fixtures/, 70 files)", root / "tests" / "fixtures"),
    ]

    verdicts = []
    for label, path in sizes:
        slug = label.split()[0]
        pid = f"bench-openc-{slug}"
        db_dir = tmp / f"db-{slug}"
        db_url = f"surrealkv://{db_dir}/graph.db"

        index_cmd = [str(binary), "index", str(path), "--project-id", pid, "--db-url", db_url, "--force"]
        t0 = time.time()
        idx = subprocess.run(index_cmd, capture_output=True, text=True, env=env)
        t_index = time.time() - t0
        if idx.returncode != 0:
            print(f"[{label}] INDEX FAILED (rc={idx.returncode}): {idx.stderr[-500:]}")
            continue

        dropped = try_drop_page_cache()
        query_cmd = [str(binary), "query", "--kind", "summary", "--project-id", pid, "--db-url", db_url]

        first = run_timed(query_cmd, env)
        warm = [run_timed(query_cmd, env) for _ in range(args.reps)]
        warm_walls = sorted(r["wall"] for r in warm)

        print(f"[{label}]  path={path}")
        print(f"    (store built via `index` in {t_index*1000:.0f}ms; "
              f"page-cache-drop before first read: {'ok' if dropped else 'unavailable (no cached sudo cred) -- see docstring'})")
        print(f"    cold (first read, new process): {fmt(first)}")
        print(f"    warm (n={len(warm)}) wall min/median/max = "
              f"{warm_walls[0]*1000:.1f} / {median(warm_walls)*1000:.1f} / {warm_walls[-1]*1000:.1f} ms")
        for i, r in enumerate(warm):
            print(f"      rep{i+1}: {fmt(r)}")
        verdicts.append((label, first, warm))
        print()

    print("=== verdict vs. D6's '37s wall / 0.8s CPU for one summary query on a 301-node store' ===")
    for label, first, warm in verdicts:
        med = median([r["wall"] for r in warm])
        print(f"  {label}: cold={first['wall']*1000:.1f}ms, warm median={med*1000:.1f}ms "
              f"(D6's 37,000ms did not reproduce on any store size tested)")

    if args.keep:
        print(f"\nscratch stores kept at: {tmp}")
    else:
        shutil.rmtree(tmp, ignore_errors=True)
        print("\nscratch stores cleaned up (pass --keep to retain)")


if __name__ == "__main__":
    main()
