#!/usr/bin/env bash
# perf/bench.sh — compare herdr builds under a lute-like load on this machine.
#
#   perf/bench.sh [options] LABEL=BINARY [LABEL=BINARY ...]
#
# The first build is the baseline; every other build is compared with it, and
# the script exits 1 if any of them regressed. A label starting with
# "upstream" marks a build without the fork's mirrors and feed: the scenarios
# that need them are skipped for it, and it is compared on the rest.
#
# Scenarios (each starts a fresh server, then measures CPU over --windows
# windows of --window seconds and reports the median):
#   idle     the agent fleet idle, one busy agent on screen, a client attached
#   mirrors  idle plus a mirrored host session, a few of its agents busy (fork)
#   feed     mirrors plus `herdr agent feed`, the way a Mac mirrors lute (fork)
#   busy     idle plus 12 busy agents tiled on screen: what drawing costs
#   handoff  start the baseline, live-handoff to each fork build, check that
#            every pane survives with its status, then measure (fork)
#
# Options:
#   --scenarios LIST   comma-separated (default: idle,mirrors,feed,busy,handoff)
#   --scale lute|small lute = 45 spaces x 3 agents + 48 mirrored (default);
#                      small = 10 x 2 + 12, for trying the script out
#   --windows N        measurement windows (default 3)
#   --window SECONDS   window length (default 10)
#   --settle SECONDS   wait after setup before measuring (default 20)
#   --work DIR         scratch directory (default ~/herdr-perf)
#
# Builds draw lute's sidebar layout (config/bench.toml), or, when an upstream
# build is in the run, a layout every build accepts (config/bench-common.toml).
#
# It runs its own sessions ("perfbench" and "perfhost") and its own tmux
# server ("-L herdr-perf"), and stops only processes started from copies of
# the binaries under --work, so it can share a machine with a live herdr.
# Needs bash, python3 and tmux. See perf/README.md.
set -euo pipefail

PERF_DIR=$(cd "$(dirname "$0")" && pwd)
SCENARIOS=idle,mirrors,feed,busy,handoff
SCALE=lute
WINDOWS=3
WINDOW=10
SETTLE=20
WORK=$HOME/herdr-perf
LABELS=()
SOURCES=()

while [ $# -gt 0 ]; do
  case $1 in
    --scenarios) SCENARIOS=$2; shift 2 ;;
    --scale) SCALE=$2; shift 2 ;;
    --windows) WINDOWS=$2; shift 2 ;;
    --window) WINDOW=$2; shift 2 ;;
    --settle) SETTLE=$2; shift 2 ;;
    --work) WORK=$2; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *=*) LABELS+=("${1%%=*}"); SOURCES+=("${1#*=}"); shift ;;
    *) echo "unknown argument: $1 (expected LABEL=BINARY)" >&2; exit 2 ;;
  esac
done
[ ${#LABELS[@]} -ge 1 ] || { echo "give at least one LABEL=BINARY" >&2; exit 2; }

case $SCALE in
  lute) SPACES=45; PER_SPACE=3; HOST_IDLE=44; HOST_BUSY=4 ;;
  small) SPACES=10; PER_SPACE=2; HOST_IDLE=10; HOST_BUSY=2 ;;
  *) echo "unknown --scale $SCALE" >&2; exit 2 ;;
esac

# A run with an upstream build in it uses a config every build accepts.
BENCH_CONFIG=$PERF_DIR/config/bench.toml
for label in "${LABELS[@]}"; do
  [[ $label == upstream* ]] && BENCH_CONFIG=$PERF_DIR/config/bench-common.toml
done

BENCH=perfbench
HOST=perfhost
TMUX_SOCKET=herdr-perf
CLK_TCK=$(getconf CLK_TCK)

mkdir -p "$WORK/bin" "$WORK/dirs"
WORK=$(cd "$WORK" && pwd)
RUN_ID=$(date +%Y%m%d-%H%M%S)
RESULTS=$WORK/results-$RUN_ID.tsv
printf 'scenario\tlabel\ttotal_pct\tmain_pct\tfull_renders_per_s\tfull_render_avg_ms\tnote\n' > "$RESULTS"

# Each build runs from its own copy, so a process's /proc/PID/exe says which
# build it is and which processes are ours to stop.
for i in "${!LABELS[@]}"; do
  cp "${SOURCES[$i]}" "$WORK/bin/${LABELS[$i]}"
  chmod +x "$WORK/bin/${LABELS[$i]}"
done

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
is_upstream() { [[ $1 == upstream* ]]; }

# Run a build against one of our sessions, with nothing inherited from a herdr
# the script may itself be running inside.
herdr_as() {
  local session=$1 bin=$2; shift 2
  local config=$WORK/$session.toml
  env $(env | grep -oE '^HERDR_[A-Z_]+' | sed 's/^/-u /') \
    HERDR_SESSION=$session HERDR_CONFIG_PATH=$config "$WORK/bin/$bin" "$@"
}

session_dir() { echo "$HOME/.config/herdr/sessions/$1"; }

# PIDs of our processes: started from a copy under $WORK/bin, and with WORD
# in their arguments when one is given.
our_pids() {
  local word=${1:-} p exe
  for p in /proc/[0-9]*; do
    exe=$(readlink "$p/exe" 2>/dev/null) || continue
    case $exe in "$WORK/bin/"*) ;; *) continue ;; esac
    if [ -z "$word" ] || tr '\0' ' ' < "$p/cmdline" 2>/dev/null | grep -qw -- "$word"; then
      echo "${p#/proc/}"
    fi
  done
}

session_of() { tr '\0' '\n' < "/proc/$1/environ" 2>/dev/null | sed -n 's/^HERDR_SESSION=//p'; }

server_pid() {
  local p
  for p in $(our_pids server); do
    [ "$(session_of "$p")" = "$1" ] && { echo "$p"; return; }
  done
}

stop_all() {
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  local p
  for p in $(our_pids); do kill "$p" 2>/dev/null || true; done
  sleep 2
  for p in $(our_pids); do kill -9 "$p" 2>/dev/null || true; done
  for s in $BENCH $HOST; do
    rm -f "$(session_dir $s)/session.json"
  done
}
trap stop_all EXIT

start_server() {
  local session=$1 bin=$2 config=$3
  cp "$config" "$WORK/$session.toml"
  mkdir -p "$(session_dir "$session")"
  : > "$(session_dir "$session")/herdr-server.log"
  ( env $(env | grep -oE '^HERDR_[A-Z_]+' | sed 's/^/-u /') \
      HERDR_SESSION=$session HERDR_CONFIG_PATH=$WORK/$session.toml HERDR_RENDER_PROF=1 \
      setsid nohup "$WORK/bin/$bin" server > "$WORK/$session.out" 2>&1 & )
  for _ in $(seq 1 50); do
    if herdr_as "$session" "$bin" workspace list > /dev/null 2>&1; then
      if grep -q "config parse error" "$(session_dir "$session")/herdr-server.log"; then
        echo "$bin rejected $config; see $(session_dir "$session")/herdr-server.log" >&2
        return 1
      fi
      return 0
    fi
    sleep 0.2
  done
  echo "server for $session ($bin) did not come up; see $WORK/$session.out" >&2
  return 1
}

json_field() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)"; }

# Create workspaces of agents. Prints the pane ids, one per line.
make_spaces() {
  local session=$1 bin=$2 count=$3 per=$4 prefix=$5 i k root last pane
  for i in $(seq 1 "$count"); do
    mkdir -p "$WORK/dirs/$prefix$i"
    root=$(herdr_as "$session" "$bin" workspace create --cwd "$WORK/dirs/$prefix$i" \
      --label "$prefix$i" --no-focus | json_field 'd["result"]["root_pane"]["pane_id"]')
    echo "$root"; last=$root
    for k in $(seq 2 "$per"); do
      pane=$(herdr_as "$session" "$bin" pane split "$last" --direction down \
        --cwd "$WORK/dirs/$prefix$i" | json_field 'd["result"]["pane"]["pane_id"]')
      echo "$pane"; last=$pane
    done
  done
}

# Panes need their shell up before `pane run`; create them all first, then run.
run_agents() {
  local session=$1 bin=$2 kind=$3; shift 3
  sleep 3
  local p
  for p in "$@"; do
    herdr_as "$session" "$bin" pane run "$p" "exec $PERF_DIR/agents/$kind/claude" > /dev/null
  done
}

attach_client() {
  local bin=$1
  tmux -L "$TMUX_SOCKET" new-session -d -x 250 -y 70 \
    "env HERDR_SESSION=$BENCH HERDR_CONFIG_PATH=$WORK/$BENCH.toml $WORK/bin/$bin"
}

# The fleet every scenario starts from: idle agents in background spaces, one
# busy agent in the focused space, and a client looking at it.
setup_fleet() {
  local bin=$1 extra_busy=${2:-0}
  start_server "$BENCH" "$bin" "$BENCH_CONFIG"
  mapfile -t idle < <(make_spaces "$BENCH" "$bin" "$SPACES" "$PER_SPACE" ws)
  mkdir -p "$WORK/dirs/busy"
  local root
  root=$(herdr_as "$BENCH" "$bin" workspace create --cwd "$WORK/dirs/busy" --label busy --focus \
    | json_field 'd["result"]["root_pane"]["pane_id"]')
  local busy=("$root") last=$root pane i
  for i in $(seq 1 "$extra_busy"); do
    pane=$(herdr_as "$BENCH" "$bin" pane split "$last" \
      --direction "$([ $((i % 2)) = 1 ] && echo right || echo down)" --cwd "$WORK/dirs/busy" \
      | json_field 'd["result"]["pane"]["pane_id"]')
    busy+=("$pane"); last=$pane
  done
  run_agents "$BENCH" "$bin" idle "${idle[@]}"
  run_agents "$BENCH" "$bin" busy "${busy[@]}"
  attach_client "$bin"
}

setup_host_and_mirror() {
  local bin=$1
  start_server "$HOST" "$bin" "$PERF_DIR/config/host.toml"
  mapfile -t host < <(make_spaces "$HOST" "$bin" $((HOST_IDLE + HOST_BUSY)) 1 h)
  run_agents "$HOST" "$bin" busy "${host[@]:0:$HOST_BUSY}"
  run_agents "$HOST" "$bin" idle "${host[@]:$HOST_BUSY}"
  sed "s/SESSION_HOST/$HOST/" "$PERF_DIR/config/mirror.toml" >> "$WORK/$BENCH.toml"
  herdr_as "$BENCH" "$bin" server reload-config > /dev/null
}

start_feed() {
  local bin=$1
  ( env $(env | grep -oE '^HERDR_[A-Z_]+' | sed 's/^/-u /') \
      HERDR_SESSION=$BENCH HERDR_CONFIG_PATH=$WORK/$BENCH.toml \
      setsid nohup "$WORK/bin/$bin" agent feed > "$WORK/feed.out" 2> "$WORK/feed.err" & )
}

ticks() { awk '{print $14 + $15}' "$1"; }

# Measure the bench server: prints "total main full_renders_per_s full_render_avg_ms",
# each the median over the windows.
measure() {
  local pid log w a b c d off
  pid=$(server_pid "$BENCH")
  [ -n "$pid" ] || { echo "no bench server" >&2; return 1; }
  log=$(session_dir "$BENCH")/herdr-server.log
  local totals=() mains=() renders=() virt=()
  for w in $(seq 1 "$WINDOWS"); do
    off=$(stat -c %s "$log")
    a=$(ticks "/proc/$pid/stat"); b=$(ticks "/proc/$pid/task/$pid/stat")
    sleep "$WINDOW"
    c=$(ticks "/proc/$pid/stat"); d=$(ticks "/proc/$pid/task/$pid/stat")
    totals+=($(( (c - a) * 100 / CLK_TCK / WINDOW )))
    mains+=($(( (d - b) * 100 / CLK_TCK / WINDOW )))
    read -r r v < <(tail -c +$((off + 1)) "$log" | python3 "$PERF_DIR/render_prof.py" "$WINDOW")
    renders+=("$r"); virt+=("$v")
  done
  echo "$(median "${totals[@]}") $(median "${mains[@]}") $(median "${renders[@]}") $(median "${virt[@]}")"
}

# Median of the numeric arguments; "-" when there are none.
median() {
  printf '%s\n' "$@" | grep -v '^-$' | sort -g \
    | awk '{a[NR]=$1} END {print NR ? a[int((NR + 1) / 2)] : "-"}'
}

record() {
  local scenario=$1 label=$2 note=${3:-}
  local total main renders virt
  read -r total main renders virt < <(measure)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$scenario" "$label" "$total" "$main" "$renders" "$virt" "$note" >> "$RESULTS"
  log "$scenario/$label: total ${total}% main ${main}% full renders ${renders}/s full render ${virt}ms $note"
}

# What the server sees, so a setup that did not take (agents not detected, no
# client) shows in the results instead of passing as a quiet build.
fleet_note() {
  # The client runs as the tmux session's only process: no session, no client.
  tmux -L "$TMUX_SOCKET" has-session 2>/dev/null && printf 'client=up ' || printf 'client=GONE '
  herdr_as "$BENCH" "$1" pane list | python3 -c '
import sys, json, collections
panes = json.load(sys.stdin)["result"]["panes"]
c = collections.Counter(p.get("agent_status", "?") for p in panes)
print("panes=%d %s" % (len(panes), " ".join("%s=%d" % kv for kv in sorted(c.items()))))'
}

pane_snapshot() {
  herdr_as "$BENCH" "$1" pane list \
    | python3 -c 'import sys,json; [print(p["pane_id"], p["agent_status"]) for p in json.load(sys.stdin)["result"]["panes"] if p.get("mirror_origin") is None]' \
    | sort
}

run_scenario() {
  local scenario=$1 label=$2
  stop_all
  case $scenario in
    idle) setup_fleet "$label" ;;
    busy) setup_fleet "$label" 12 ;;
    mirrors) setup_fleet "$label"; setup_host_and_mirror "$label" ;;
    feed) setup_fleet "$label"; setup_host_and_mirror "$label"; start_feed "$label" ;;
  esac
  sleep "$SETTLE"
  local note
  note=$(fleet_note "$label")
  if [ "$scenario" = feed ]; then
    local feed
    feed=$(for p in $(our_pids feed); do [ "$(session_of "$p")" = "$BENCH" ] && echo "$p"; done | head -1)
    note="$note feed_threads=$(ls "/proc/$feed/task" 2>/dev/null | wc -l)"
  fi
  record "$scenario" "$label" "$note"
}

run_handoff() {
  local base=$1 label=$2 before after
  stop_all
  setup_fleet "$base"
  sleep "$SETTLE"
  before=$(pane_snapshot "$base")
  local started=$SECONDS
  if ! herdr_as "$BENCH" "$base" server live-handoff --import-exe "$WORK/bin/$label" > "$WORK/handoff.out" 2>&1; then
    printf 'handoff\t%s\t-\t-\t-\t-\tFAILED: %s\n' "$label" "$(tail -1 "$WORK/handoff.out")" >> "$RESULTS"
    log "handoff/$label: FAILED"
    return
  fi
  local took=$((SECONDS - started))
  sleep 5
  attach_client "$label"   # a handoff drops attached clients
  sleep "$SETTLE"
  after=$(pane_snapshot "$label")
  local note="handoff ${took}s, $(echo "$before" | wc -l) panes"
  if [ "$before" = "$after" ]; then note="$note kept"; else note="$note CHANGED"; fi
  record handoff "$label" "$note"
}

IFS=, read -r -a SCENARIO_LIST <<< "$SCENARIOS"
BASE=${LABELS[0]}
for scenario in "${SCENARIO_LIST[@]}"; do
  if [ "$scenario" = handoff ]; then
    is_upstream "$BASE" && continue
    for label in "${LABELS[@]:1}"; do
      is_upstream "$label" || run_handoff "$BASE" "$label"
    done
    continue
  fi
  for label in "${LABELS[@]}"; do
    case $scenario in mirrors|feed) is_upstream "$label" && continue ;; esac
    log "scenario $scenario: $label"
    run_scenario "$scenario" "$label"
  done
done
stop_all
trap - EXIT

python3 "$PERF_DIR/report.py" "$RESULTS" "$BASE"
