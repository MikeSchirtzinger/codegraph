#!/usr/bin/env bash
# Replay the continuedev/continue false negative end to end.
#
# The audit recorded in specs/receipts/extractor-gaps-20260914.md
# found a real dangling reference the structural gate did not report. Commit
# 5b5532f7 deletes extensions/cli/src/tools/searchAndReplace/, which exported
# searchAndReplaceInFileTool, and leaves extensions/cli/src/tools/
# preprocess.test.ts importing that symbol and calling it five times. The
# project's own next commit, fea84252 "fix: broken test", deletes the import
# and the test block, so the repository adjudicates the defect itself.
#
# Three gaps had to close before the gate could see it: an exported const
# bound to an object literal was not a definition (G1), code inside describe
# and it callbacks produced no edges (G2), and the stale scan matched only on
# the bare tail of a capture, so a deleted symbol in receiver position never
# matched (G3). With all three fixed this script must print a stale reference
# from preprocess.test.ts to searchAndReplaceInFileTool.
#
# Network: this clones from github.com, so it lives here rather than in
# tests/. The suite under tests/ never reaches the network; scripts/corpus/
# run.sh sets the same precedent for corpus work that does.
#
# usage: scripts/replay/continue-5b5532f7.sh [--binary PATH] [--gate PATH]
#                                            [--work DIR] [--keep]
#
# Exits 0 when the gate reports the stale reference, 1 when it does not.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

BINARY="$REPO_ROOT/target/release/codegraph"
GATE="$REPO_ROOT/target/release/examples/gate_verdict"
WORK=""
KEEP=0

PARENT=77288b0e41f7965f3371cb9c3d25f82f60315c68
REPLAY=5b5532f7e7de7a51e872d04ac2e7b2379c6d9eb1
PROJECT=replay-continue-5b5532f7
SCOPE=extensions/cli/src
SYMBOL=searchAndReplaceInFileTool

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="$2"; shift 2 ;;
    --gate) GATE="$2"; shift 2 ;;
    --work) WORK="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,27p' "$0" >&2; exit 64 ;;
    *) echo "unrecognized argument: $1" >&2; exit 64 ;;
  esac
done

for tool in "$BINARY" "$GATE"; do
  if [ ! -x "$tool" ]; then
    echo "not executable: $tool" >&2
    echo "build them first: cargo build --release && cargo build --release --example gate_verdict" >&2
    exit 64
  fi
done

if [ -z "$WORK" ]; then
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/replay-continue-XXXXXX")"
fi
mkdir -p "$WORK"
CHECKOUT="$WORK/continue"
DB="surrealkv://$WORK/db/graph.db"

cleanup() {
  if [ "$KEEP" -eq 0 ] && [ -n "${WORK:-}" ]; then
    rm -rf "$WORK"
  else
    echo "work kept at $WORK"
  fi
}
trap cleanup EXIT

echo "== fetching continuedev/continue at $PARENT and $REPLAY"
if [ ! -d "$CHECKOUT/.git" ]; then
  git init -q "$CHECKOUT" || exit 1
  git -C "$CHECKOUT" remote add origin https://github.com/continuedev/continue.git || exit 1
fi
git -C "$CHECKOUT" fetch -q --depth 1 origin "$PARENT" "$REPLAY" || {
  echo "fetch failed; this script needs network access to github.com" >&2
  exit 1
}

echo "== indexing the parent tree"
git -C "$CHECKOUT" checkout -q --detach "$PARENT" || exit 1
"$BINARY" index "$CHECKOUT/$SCOPE" --project-id "$PROJECT" \
  --tier full --languages typescript --db-url "$DB" || exit 1

echo "== applying $REPLAY and re-indexing incrementally"
# Deliberately not --force: deletion tracking only runs on an incremental
# re-index, and the deleted symbol it records is what the gate queries.
git -C "$CHECKOUT" checkout -q --detach "$REPLAY" || exit 1
"$BINARY" index "$CHECKOUT/$SCOPE" --project-id "$PROJECT" \
  --tier full --languages typescript --db-url "$DB" || exit 1

echo "== gate verdict on the deleted paths"
VERDICT="$WORK/verdict.json"
"$GATE" "$DB" "$PROJECT" \
  tools/searchAndReplace/index.ts tools/searchAndReplace/parseArgs.ts \
  tools/searchAndReplace/parseBlock.ts tools/searchAndReplace/findSearchMatch.ts \
  | tee "$VERDICT"

echo "== symbol query for $SYMBOL"
"$BINARY" query --kind rdeps --name "$SYMBOL" --explain --json \
  --project-id "$PROJECT" --db-url "$DB"

if grep -q 'preprocess.test.ts' "$VERDICT"; then
  echo "PASS: the gate flags preprocess.test.ts"
  exit 0
fi

echo "FAIL: no stale reference from preprocess.test.ts in the verdict" >&2
exit 1
