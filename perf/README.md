# perf: fleet-scale CPU bench

Compares herdr builds under the load lute carries: about 135 idle agents, one
or more busy ones on screen, mirrors of another session's agents, an attached
client and `herdr agent feed`. Upstream's `just bench-render-scale` and
`just bench-release-smoke` measure rendering with a handful of panes; this
measures the fork's whole server with a fleet, which is where its idle-CPU
regressions have come from.

## Running it

From a checkout, on the bench machine (emily) over ssh:

```bash
perf/remote.sh emily deployed=$HOME/.local/bin/herdr candidate=target/release/herdr
```

or directly on the machine running it:

```bash
perf/bench.sh deployed=/path/to/herdr candidate=/path/to/other/herdr
```

The first build is the baseline. The report prints a table and lists
regressions: a build whose total or main-thread CPU exceeds the baseline's by
more than 25% plus 3 points in any scenario. The script exits 1 when there is
one, or when a handoff fails or loses panes. A lute-scale run of all five
scenarios against two builds takes about 12 minutes; `--scale small` with
shorter `--settle` and `--window` is for checking the script itself.

To compare with upstream, label the upstream build `upstream`:

```bash
perf/remote.sh emily --scenarios idle,busy candidate=target/release/herdr upstream=/path/to/upstream/herdr
```

Upstream has no mirrors or feed, so those scenarios and the handoff skip it.
With an upstream build in the run, every build uses `config/bench-common.toml`,
a sidebar layout upstream accepts too, so they draw the same thing. Current
upstream needs Zig 0.16 to build (`ZIG=/path/to/zig-0.16.0/zig cargo build
--release`); the fork is on 0.15.2.

## Scenarios

| scenario | load | fork only |
|---|---|---|
| idle | 45 spaces x 3 idle agents, one busy agent on screen, client attached | |
| mirrors | idle, plus 48 mirrored agents from a second session, 4 of them busy | yes |
| feed | mirrors, plus `herdr agent feed` on the bench session | yes |
| busy | idle, plus 12 more busy agents tiled on screen | |
| handoff | idle on the baseline, `live-handoff` to the build, panes compared | yes |

Each scenario starts a fresh server, waits `--settle` seconds, then measures
`--windows` windows of `--window` seconds and reports medians:

- **total %, main %**: the server's CPU, all threads and the main thread.
  The main thread is the one that saturates.
- **full renders/s, render ms**: from the server's render profiler
  (`HERDR_RENDER_PROF=1`). A full render redraws the whole UI; output in a
  visible pane is normally drawn with a much cheaper retained patch. "-" means
  nothing was fully rendered.
- **note**: what the server saw (panes, agent states, whether the client is
  still attached), the feed's thread count, and the handoff's outcome. Check it
  before trusting a quiet result: a build that failed to detect its agents is
  quiet for the wrong reason.

The fake agents in `agents/` are scripts named `claude` so herdr detects them.
`busy/claude` redraws a spinner about 12 times a second, like Claude's own.

## On the machine

It runs sessions `perfbench` and `perfhost` and a tmux server `-L herdr-perf`,
and stops only processes started from the copies it makes under `--work`
(default `~/herdr-perf`), so a live herdr on the same machine is left alone.
Results stay in `--work` as `results-*.tsv`.

## History

- 2026-10-11 (lute scale, emily): the build before the idle-CPU fix
  (`dc107877`) against the fix: mirrors 71% vs 14% total, feed 115% vs 14%
  with 187 feed threads vs 4. The bench flags both.
- 2026-10-11, against upstream 0.9.3 (`2563803`): idle 11% / 4% main for the
  fork vs 6% / 0% upstream. Upstream draws an idle fleet with retained patches
  alone; the fork still does about 0.3 full renders a second at 12-16ms each.
  busy 29% / 20% vs 39% / 20%.
- 2026-10-11, lute's deployed build (`49c52325`) against the census commit:
  no regressions; every scenario within a point or two, handoff kept all 136
  panes. Of the 13 busy agents in `busy`, herdr detects 7 as working at that
  tile size, in every build alike.
