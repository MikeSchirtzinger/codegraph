#!/usr/bin/env python3
"""Serve-mode (MCP stdio) per-request latency, and CLI-reopen vs. serve-amortized.

Speaks raw MCP-over-stdio JSON-RPC directly (newline-delimited JSON --
confirmed by reading rmcp 1.8.0's transport/async_rw.rs, which frames on
`b'\\n'`; no Content-Length/LSP-style framing). No MCP client library
dependency; stdlib only.

Two things get measured against each of two indexed targets (the required
polyglot fixture, and this repo's own self-indexed src/ for a larger-store
comparison):

  1. serve-mode: one long-lived `codegraph serve` process, >=50 requests
     per tool for 3 representative tools (search / impact / architecture),
     p50/p95 computed per tool. The store is opened exactly once for the
     whole run (`db::connect` pays its cost a single time) -- this is the
     "amortized" path specs/resolution-layer-v1.md's R5 phase blesses.

  2. CLI-reopen: the same query, but as N separate `codegraph query`
     process invocations -- each one reopens the store from scratch (the
     unamortized path every CLI call takes today). Adaptively capped (see
     `bench_cli_reopen`) so a slow store-open doesn't turn this into a
     multi-minute run -- the point is the delta, not statistical power on
     a number we already characterize in store_open_cost.py.

Repro:
  cargo build --release
  python3 scripts/bench/mcp_latency.py [--requests N] [--cli-requests N] [--keep]
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
import shutil
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
        print(f"[mcp_latency] {b} not found -- building (cargo build --release)...", file=sys.stderr)
        subprocess.run(["cargo", "build", "--release"], cwd=root, check=True)
    if not b.exists():
        sys.exit(f"cargo build finished but {b} still missing -- check CARGO_TARGET_DIR")
    return b


class McpClient:
    """Minimal MCP-over-stdio JSON-RPC client -- just enough to initialize
    and call tools/call. Newline-delimited JSON, one message per line."""

    def __init__(self, cmd, env):
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1, env=env,
        )
        self._id = 0

    def _send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def _recv(self):
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError("codegraph serve closed stdout unexpectedly")
        return json.loads(line)

    def request(self, method, params):
        self._id += 1
        rid = self._id
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        resp = self._recv()
        if resp.get("id") != rid:
            raise RuntimeError(f"MCP response id mismatch: sent {rid}, got {resp}")
        return resp

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def initialize(self):
        self.request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "codegraph-bench", "version": "1.0.0"},
        })
        self.notify("notifications/initialized")

    def call_tool(self, name, arguments):
        t0 = time.perf_counter()
        resp = self.request("tools/call", {"name": name, "arguments": arguments})
        elapsed = time.perf_counter() - t0
        if "error" in resp:
            raise RuntimeError(f"tool call error: {resp['error']}")
        return elapsed

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def percentile(sorted_vals, p):
    if not sorted_vals:
        return float("nan")
    k = (len(sorted_vals) - 1) * p
    f, c = int(k), min(int(k) + 1, len(sorted_vals) - 1)
    if f == c:
        return sorted_vals[f]
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def summarize(label, samples):
    s = sorted(samples)
    p50, p95 = percentile(s, 0.50) * 1000, percentile(s, 0.95) * 1000
    mean = statistics.mean(s) * 1000
    print(f"    {label:<22} n={len(s):<4} mean={mean:7.2f}ms  p50={p50:7.2f}ms  p95={p95:7.2f}ms  "
          f"min={s[0]*1000:7.2f}ms  max={s[-1]*1000:7.2f}ms")


TOOL_CALLS = {
    "codegraph_search": {"query": "connect"},
    "codegraph_impact": {"name": "connect"},
    "codegraph_architecture": {},
}


def bench_serve(binary, db_url, project_id, env, n_requests):
    cmd = [str(binary), "serve", "--project-id", project_id, "--db-url", db_url]
    client = McpClient(cmd, env)
    try:
        client.initialize()
        for tool, tool_args in TOOL_CALLS.items():
            client.call_tool(tool, tool_args)  # warm-up, discarded
        print("  [serve-mode MCP: one long-lived process, store opened once, amortized]")
        for tool, tool_args in TOOL_CALLS.items():
            samples = [client.call_tool(tool, tool_args) for _ in range(n_requests)]
            summarize(tool, samples)
    finally:
        client.close()


def bench_cli_reopen(binary, db_url, project_id, env, target_requests, budget_s=30.0):
    print("  [CLI-reopen: one fresh process per request, store reopened every time]")
    cli_calls = {
        "query search": ["query", "--kind", "search", "--name", "connect",
                          "--project-id", project_id, "--db-url", db_url],
        "query rdeps(impact)": ["query", "--kind", "rdeps", "--name", "connect",
                                 "--project-id", project_id, "--db-url", db_url],
    }
    for label, cli_args in cli_calls.items():
        cmd = [str(binary)] + cli_args
        t0 = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, text=True, env=env)
        probe = time.perf_counter() - t0
        if p.returncode != 0:
            print(f"    {label}: FAILED (rc={p.returncode}): {p.stderr[-300:]}")
            continue
        n = target_requests if probe <= 0 else max(3, min(target_requests, int(budget_s / probe)))
        if n < target_requests:
            print(f"    ({label}: probe call took {probe*1000:.0f}ms; capping to n={n} to stay "
                  f"under a {budget_s:.0f}s budget for this section -- store_open_cost.py has the "
                  f"full cold/warm breakdown at higher rep counts)")
        samples = [probe]
        for _ in range(n - 1):
            t0 = time.perf_counter()
            subprocess.run(cmd, capture_output=True, text=True, env=env)
            samples.append(time.perf_counter() - t0)
        summarize(label, samples)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--requests", type=int, default=60,
                    help="requests per tool for serve-mode p50/p95 (default 60; must be >=50)")
    ap.add_argument("--cli-requests", type=int, default=15,
                    help="target requests for the CLI-reopen comparison (default 15; auto-capped if slow)")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    if args.requests < 50:
        sys.exit("--requests must be >= 50")

    print(f"Repro: python3 scripts/bench/mcp_latency.py --requests {args.requests} "
          f"--cli-requests {args.cli_requests}" + (" --keep" if args.keep else ""))

    root = repo_root()
    binary = ensure_binary(root)
    env = dict(os.environ)
    env["RUST_LOG"] = "codegraph=warn"  # quiet; stderr is DEVNULL'd for serve anyway

    tmp = Path(tempfile.mkdtemp(prefix="codegraph-bench-mcp-"))
    print(f"binary:      {binary}")
    print(f"scratch dir: {tmp}\n")

    targets = [
        ("polyglot", root / "tests" / "fixtures" / "polyglot"),
        ("self-src", root / "src"),
    ]
    for label, path in targets:
        pid = f"bench-mcp-{label}"
        db_dir = tmp / f"db-{label}"
        db_url = f"surrealkv://{db_dir}/graph.db"
        idx_cmd = [str(binary), "index", str(path), "--project-id", pid, "--db-url", db_url, "--force"]
        p = subprocess.run(idx_cmd, capture_output=True, text=True, env=env)
        if p.returncode != 0:
            print(f"[{label}] INDEX FAILED: {p.stderr[-500:]}")
            continue

        print(f"=== target: {label} ({path}) ===")
        bench_serve(binary, db_url, pid, env, args.requests)
        bench_cli_reopen(binary, db_url, pid, env, args.cli_requests)
        print()

    if args.keep:
        print(f"scratch stores kept at: {tmp}")
    else:
        shutil.rmtree(tmp, ignore_errors=True)
        print("scratch stores cleaned up (pass --keep to retain)")


if __name__ == "__main__":
    main()
