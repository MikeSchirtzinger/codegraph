#!/usr/bin/env bash
# Run codegraph's first-run experience against a corpus of public repositories
# it has never seen, and record what actually happens.
#
# See scripts/corpus/README.md for how to run this and what it proves.
#
# Deliberately not `set -e`: one repo failing must not abort the other nine.
# Each repo's outcome is recorded and the script's own exit code is decided
# at the very end from those outcomes.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat >&2 <<'EOF'
usage: run.sh [--repos FILE] [--binary PATH] [--corpus-dir DIR] [--out DIR]

  --repos FILE       pipe-delimited repo list (default: scripts/corpus/repos.txt,
                      next to this script)
  --binary PATH       the codegraph binary to exercise (default: <repo root>/
                      target/release/codegraph if it exists; otherwise required)
  --corpus-dir DIR    where repos get cloned and reused across runs (default:
                      $HOME/.cache/codegraph-corpus). Not wiped between runs:
                      delete a specific repo's subdirectory to force a fresh
                      clone of just that one.
  --out DIR           where this run's logs and summary table are written
                      (default: scripts/corpus/out/run-<UTC timestamp>, always
                      a fresh directory)

Every run is logged to <out>/run.log and <out>/<repo>/*.log. Exits non-zero
if any repo's first `codegraph index` failed.
EOF
  exit 64
}

REPOS_FILE="$SCRIPT_DIR/repos.txt"
BINARY=""
CORPUS_DIR="${CODEGRAPH_CORPUS_DIR:-$HOME/.cache/codegraph-corpus}"
OUT_DIR="$SCRIPT_DIR/out/run-$(date -u +%Y%m%dT%H%M%SZ)"

while [ $# -gt 0 ]; do
  case "$1" in
    --repos) REPOS_FILE="$2"; shift 2 ;;
    --binary) BINARY="$2"; shift 2 ;;
    --corpus-dir) CORPUS_DIR="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unrecognized argument: $1" >&2; usage ;;
  esac
done

if [ -z "$BINARY" ]; then
  REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
  if [ -x "$REPO_ROOT/target/release/codegraph" ]; then
    BINARY="$REPO_ROOT/target/release/codegraph"
  else
    echo "no --binary given and $REPO_ROOT/target/release/codegraph does not exist." >&2
    echo "Build one first (cargo build --release) or pass --binary explicitly." >&2
    exit 64
  fi
fi

[ -f "$REPOS_FILE" ] || { echo "no such repos file: $REPOS_FILE" >&2; exit 1; }
[ -x "$BINARY" ] || { echo "not an executable file: $BINARY" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required (brew install jq) and is not on PATH" >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo "git is required and is not on PATH" >&2; exit 1; }

BINARY="$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"
REPOS_FILE="$(cd "$(dirname "$REPOS_FILE")" && pwd)/$(basename "$REPOS_FILE")"
mkdir -p "$CORPUS_DIR" "$OUT_DIR"
CORPUS_DIR="$(cd "$CORPUS_DIR" && pwd)"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
RUN_LOG="$OUT_DIR/run.log"
RESULTS_TSV="$OUT_DIR/results.tsv"

log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$RUN_LOG"; }

log "codegraph corpus run starting"
log "binary:      $BINARY ($("$BINARY" --version 2>&1 | head -1))"
log "repos file:  $REPOS_FILE"
log "corpus dir:  $CORPUS_DIR"
log "output dir:  $OUT_DIR"
log "host uptime: $(uptime)"

# One command, timed and logged in full: stdout, stderr, exit code, wall
# time, user/sys CPU and peak RSS all land in $1. The real exit code of the
# command survives (macOS `/usr/bin/time` exits with its child's status).
run_step() {
  local logfile="$1"; shift
  {
    echo "\$ $*"
    echo "--- start: $(date -u +%FT%TZ) ---"
  } > "$logfile"
  /usr/bin/time -l "$@" >> "$logfile" 2>&1
  local status=$?
  {
    echo "--- exit: $status ---"
    echo "--- end: $(date -u +%FT%TZ) ---"
  } >> "$logfile"
  return $status
}

# Pull one field out of a `time -l` log. macOS's BSD time(1) prints lines
# like "     1.23 real         0.98 user         0.15 sys" and
# "   1234567  maximum resident set size".
time_field() {
  local logfile="$1" field="$2"
  grep -oE "[0-9.]+ $field" "$logfile" 2>/dev/null | head -1 | awk '{print $1}'
}

maxrss_bytes() {
  grep -E 'maximum resident set size' "$1" 2>/dev/null | head -1 | awk '{print $1}'
}

# The first line of `codegraph query --kind hubs --json` output is a bare
# JSON array; nothing else in the log starts with '['.
json_line() {
  grep -E '^\[|^\{' "$1" 2>/dev/null | head -1
}

process_repo() {
  local name="$1" langmix="$2" url="$3" sha_hint="$4" probe="$5" ignore_dir="$6"
  local repo_dir="$CORPUS_DIR/$name"
  local log_dir="$OUT_DIR/$name"
  mkdir -p "$log_dir"

  log "== $name ($langmix) =="

  if [ ! -d "$repo_dir/.git" ]; then
    if ! run_step "$log_dir/00-clone.log" git clone --depth 1 "$url" "$repo_dir"; then
      log "$name: CLONE FAILED, see $log_dir/00-clone.log"
      printf '%s\tCLONE-FAIL\n' "$name" >> "$RESULTS_TSV"
      return 1
    fi
  else
    echo "reusing existing clone at $repo_dir (delete it to force a fresh clone)" >> "$log_dir/00-clone.log"
  fi
  local achieved_sha
  achieved_sha="$(git -C "$repo_dir" rev-parse HEAD)"
  echo "sha=$achieved_sha (surveyed at $sha_hint)" >> "$log_dir/00-clone.log"

  # Ignore-rule canary: prove a directory of this name does NOT get indexed,
  # rather than assume it from an absence of counter-evidence.
  if [ -n "$ignore_dir" ] && [ ! -e "$log_dir/.canary-planted" ]; then
    mkdir -p "$repo_dir/$ignore_dir"
    cat > "$repo_dir/$ignore_dir/codegraph_l5_ignore_probe.py" <<'PYEOF'
def codegraph_l5_probe_function():
    """Planted by scripts/corpus/run.sh to test whether an ignored
    directory gets indexed anyway. If this symbol turns up in a query,
    it did."""
    return True
PYEOF
    touch "$log_dir/.canary-planted"
    log "$name: planted ignore-rule canary at $ignore_dir/codegraph_l5_ignore_probe.py"
  fi

  ( cd "$repo_dir" && run_step "$log_dir/01-init.log" "$BINARY" init )
  ( cd "$repo_dir" && run_step "$log_dir/02-doctor.log" "$BINARY" doctor )
  local doctor_status=$?

  uptime > "$log_dir/03-index.uptime-before.txt"
  ( cd "$repo_dir" && run_step "$log_dir/03-index.log" "$BINARY" index )
  local index_status=$?

  ( cd "$repo_dir" && run_step "$log_dir/04-query-summary.log" "$BINARY" query --kind summary )
  ( cd "$repo_dir" && run_step "$log_dir/05-query-hubs.log" "$BINARY" query --kind hubs --limit 5 )
  ( cd "$repo_dir" && run_step "$log_dir/06-query-hubs-json.log" "$BINARY" query --kind hubs --limit 1 --json )
  ( cd "$repo_dir" && run_step "$log_dir/07-query-circular.log" "$BINARY" query --kind circular )

  local top_hub
  top_hub="$(json_line "$log_dir/06-query-hubs-json.log" | jq -r '.[0].name // empty' 2>/dev/null)"
  if [ -z "$top_hub" ]; then
    echo "no hub found (empty graph, or a project this small produced none); rdeps-by-top-hub step skipped" \
      > "$log_dir/08-query-rdeps-tophub.log"
  else
    echo "top hub: $top_hub" >> "$RUN_LOG"
    ( cd "$repo_dir" && run_step "$log_dir/08-query-rdeps-tophub.log" "$BINARY" query --kind rdeps --name "$top_hub" )
  fi

  # Expected to fail: --kind rdeps with no --name must refuse, not exit 0.
  ( cd "$repo_dir" && run_step "$log_dir/09-query-rdeps-noname.log" "$BINARY" query --kind rdeps )

  ( cd "$repo_dir" && run_step "$log_dir/10-plan-lint.log" "$BINARY" plan lint )

  # May still be lane stubs at this commit; see the receipt for which.
  ( cd "$repo_dir" && run_step "$log_dir/11-landscape.log" "$BINARY" landscape )
  ( cd "$repo_dir" && run_step "$log_dir/12-plan-sync.log" "$BINARY" plan sync )

  if [ -n "$probe" ]; then
    ( cd "$repo_dir" && run_step "$log_dir/14-query-search-probe.log" "$BINARY" query --kind search --name "$probe" --json )
  fi
  if [ -n "$ignore_dir" ]; then
    ( cd "$repo_dir" && run_step "$log_dir/15-ignore-probe-search.log" "$BINARY" query --kind search --name codegraph_l5_probe_function --json )
  fi

  uptime > "$log_dir/13-index-incremental.uptime-before.txt"
  ( cd "$repo_dir" && run_step "$log_dir/13-index-incremental.log" "$BINARY" index )
  local incr_status=$?

  # Machine-extractable fields for summary.md. Anything not found in a log
  # (a stub bailed before printing it, a repo had nothing to index) comes
  # back empty and renders as "-"; that is signal, not a script bug.
  local files_line files_scanned files_indexed nodes edges resolved_pct
  local index_wall index_user resolve_phase incr_wall index_maxrss_mb
  files_line="$(grep -oE 'Files:[[:space:]]+[0-9]+ scanned, [0-9]+ indexed' "$log_dir/03-index.log" 2>/dev/null | head -1)"
  files_scanned="$(echo "$files_line" | grep -oE '[0-9]+ scanned' | awk '{print $1}')"
  files_indexed="$(echo "$files_line" | grep -oE '[0-9]+ indexed' | awk '{print $1}')"
  nodes="$(grep -oE '^[[:space:]]*Nodes:[[:space:]]+[0-9]+[[:space:]]*$' "$log_dir/03-index.log" 2>/dev/null | head -1 | grep -oE '[0-9]+')"
  edges="$(grep -oE '^[[:space:]]*Edges:[[:space:]]+[0-9]+[[:space:]]*$' "$log_dir/03-index.log" 2>/dev/null | head -1 | grep -oE '[0-9]+')"
  resolved_pct="$(grep -oE 'Resolved:[[:space:]]+[0-9]+ \([0-9.]+%\)' "$log_dir/03-index.log" 2>/dev/null | head -1 | grep -oE '\([0-9.]+%\)' | tr -d '()%')"
  resolve_phase="$(grep -oE 'resolve [0-9.]+s' "$log_dir/03-index.log" 2>/dev/null | head -1 | grep -oE '[0-9.]+')"
  index_wall="$(time_field "$log_dir/03-index.log" real)"
  index_user="$(time_field "$log_dir/03-index.log" user)"
  incr_wall="$(time_field "$log_dir/13-index-incremental.log" real)"
  index_maxrss_mb="$(maxrss_bytes "$log_dir/03-index.log")"
  if [ -n "$index_maxrss_mb" ]; then
    index_maxrss_mb="$(awk -v b="$index_maxrss_mb" 'BEGIN { printf "%.1f", b / 1048576 }')"
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$langmix" "${files_scanned:--}" "${files_indexed:--}" "${nodes:--}" "${edges:--}" \
    "${resolved_pct:--}" "${index_wall:--}" "${index_user:--}" "${resolve_phase:--}" "${incr_wall:--}" \
    "${index_maxrss_mb:--}" "$doctor_status" "$index_status" "$incr_status" \
    >> "$RESULTS_TSV"

  log "$name: index exit=$index_status incremental exit=$incr_status doctor exit=$doctor_status"
  return $index_status
}

printf 'name\tlanguage_mix\tfiles_scanned\tfiles_indexed\tnodes\tedges\tresolved_pct\tindex_wall_s\tindex_user_s\tresolve_phase_s\tincremental_wall_s\tindex_maxrss_mb\tdoctor_exit\tindex_exit\tincremental_exit\n' \
  > "$RESULTS_TSV"

OVERALL_STATUS=0
while IFS='|' read -r name langmix url sha_hint probe ignore_dir; do
  case "$name" in ''|'#'*) continue ;; esac
  if ! process_repo "$name" "$langmix" "$url" "$sha_hint" "$probe" "$ignore_dir"; then
    OVERALL_STATUS=1
  fi
done < "$REPOS_FILE"

# Render the mechanical summary table from results.tsv. This is the raw
# numbers and raw exit codes; specs/receipts/run-anywhere-corpus-20260914.md
# is the hand-written analysis that reads these same logs and explains what
# the exit codes mean (a non-zero exit on the no-name rdeps step is the
# command working correctly, not a failure, for instance).
SUMMARY_MD="$OUT_DIR/summary.md"
{
  echo "# codegraph corpus run, $(date -u +%FT%TZ)"
  echo
  echo "Binary: \`$BINARY\`"
  echo
  echo "| repo | language mix | files scanned | files indexed | nodes | edges | resolved % | index wall (s) | index user (s) | resolve phase (s) | incremental wall (s) | index peak RSS (MB) | doctor exit | index exit | incremental exit |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
  tail -n +2 "$RESULTS_TSV" | while IFS=$'\t' read -r name langmix files_scanned files_indexed nodes edges resolved_pct index_wall index_user resolve_phase incr_wall index_maxrss_mb doctor_exit index_exit incr_exit; do
    echo "| $name | $langmix | $files_scanned | $files_indexed | $nodes | $edges | $resolved_pct | $index_wall | $index_user | $resolve_phase | $incr_wall | $index_maxrss_mb | $doctor_exit | $index_exit | $incr_exit |"
  done
} > "$SUMMARY_MD"

log "summary written to $SUMMARY_MD"
log "overall exit: $OVERALL_STATUS"
exit $OVERALL_STATUS
