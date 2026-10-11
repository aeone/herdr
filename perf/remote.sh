#!/usr/bin/env bash
# perf/remote.sh HOST [bench options] LABEL=BINARY ...
#
# Runs perf/bench.sh on HOST (emily, normally): copies this directory and the
# local BINARY files there, runs the bench over ssh, and prints its report.
# Results stay on HOST under ~/herdr-perf/results-*.tsv. Options before the
# LABEL=BINARY arguments are passed to bench.sh unchanged.
#
#   perf/remote.sh emily deployed=$HOME/.local/bin/herdr candidate=target/release/herdr
set -euo pipefail
HOST=${1:?usage: perf/remote.sh HOST [options] LABEL=BINARY ...}; shift
PERF_DIR=$(cd "$(dirname "$0")" && pwd)
REMOTE=herdr-perf

ssh "$HOST" "mkdir -p $REMOTE/perf $REMOTE/incoming"
rsync -a --delete "$PERF_DIR/" "$HOST:$REMOTE/perf/"
args=()
for a in "$@"; do
  case $a in
    *=*) label=${a%%=*}; bin=${a#*=}
         rsync -a "$bin" "$HOST:$REMOTE/incoming/$label"
         args+=("$label=\$HOME/$REMOTE/incoming/$label") ;;
    *) args+=("$a") ;;
  esac
done
ssh "$HOST" "cd $REMOTE && bash perf/bench.sh --work \$HOME/$REMOTE/work ${args[*]}" < /dev/null
