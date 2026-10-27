#!/usr/bin/env bash
# Replay Claude Code transcripts into peerfs spaces and print the result tables.
# Usage: harness/measure/run.sh WORKDIR [TRANSCRIPT...]
# With no transcripts, every transcript under ~/.claude/projects is replayed, except those
# named in $MEASURE_EXCLUDE (a grep -E pattern). WORKDIR receives trees, spaces (which hold
# file contents) and the aggregate outputs out/sessions.json and out/calls.csv.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
work="$1"; shift
cargo build --release -p peerfs --manifest-path "$repo/Cargo.toml" >&2
if [ $# -eq 0 ]; then
  mapfile -t ts < <(ls ~/.claude/projects/*/*.jsonl ~/.claude/projects/*/*/subagents/*.jsonl \
    | grep -Ev "${MEASURE_EXCLUDE:-^$}")
  set -- "${ts[@]}"
fi
python3 -I "$here/measure.py" --peerfs "$repo/target/release/peerfs" \
  --work "$work/s" --out "$work/out" "$@"
python3 -I "$here/summarize.py" "$work/out"
echo
echo "### Workspaces on disk"
echo
python3 -I "$here/workspaces.py" "$@"
if [ -n "${MEASURE_BUILD:-}" ]; then
  echo
  echo "### Build churn (controlled)"
  echo
  python3 -I "$here/build_churn.py" --peerfs "$repo/target/release/peerfs" --repo "$repo" \
    --work "$work/bc"
fi
