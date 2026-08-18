# Multiplexed remote mirrors

**Status:** phase 1 landed, 2026-08-17. Phases 2-5 open.
**Why now:** lute became unusable holding 12 migrated sessions on a 64 GB, 12-core
machine. Nothing about that machine was short.

## 1. What the numbers actually say

Measured on lute, steady state, 33 panes (14 its own, 19 mirrored):

| | measured | limit | headroom |
|---|---|---|---|
| server RSS | 46 MB | 64 GB | 1400x |
| all herdr + ssh on host | 441 MB | 64 GB | 145x |
| process CPU | 108% of one core | 1200% | 11x |
| **main loop thread** | **67% of one core** | **100%** | **1.5x** |
| open fds | 480 | 1024 (default soft) | 2.1x |
| threads | 459 | 236k | irrelevant |

Two of those are binding and neither is hardware:

**The fd table.** 480 fds for 33 panes is ~14.5 per pane, and the default soft
limit is 1024. That put the ceiling at ~70 panes on a machine with 64 GB of RAM.
Yesterday the fleet ran 65 panes and leaked on top of that, so it hit the wall,
and `accept` began failing. Raising the limit to 65536 buys room; it does not
change the slope.

**One thread.** The main loop is single-threaded and already spends 67% of a core
at rest. Render, internal events, API dispatch and *accepting client connections*
are all steps in that one loop, so when it saturates, new clients cannot connect
-- which is exactly how `herdr` came to hang on lute. Eleven idle cores cannot
help: the work is not parallel.

The rest of the CPU is poll overhead: ~459 connection threads each waking every
100 ms to check whether their peer is gone, ~4,600 wakeups/sec at ~3 syscalls
each. That cost is proportional to connection count, which is proportional to
mirrored panes.

## 2. Where the cost comes from

A mirror is one *remote pane*, and today it is implemented as a full interactive
attach. Per mirrored pane, on both machines:

- an `ssh` process (its own pipes, ~5-10 MB RSS)
- a PTY pair
- a `herdr terminal attach` process on the far side
- a connection to the far side's client socket, held for the mirror's life
- a poller thread on the far side's server
- an **exclusive claim** on that terminal: `ObserveTerminal` exists in the wire
  protocol and does not claim ownership, but mirroring does not use it

Everything else follows from that one decision. Exclusivity forces the star
topology, since two machines cannot both hold a pane. Wholesale rebuilds on
reconnect and handoff turn one blip into N reconnections. Retrying an offline
host means N ssh spawns per interval, not one. `keep_offline_mirrors` is
unusable with a sleeping laptop for the same reason.

## 3. The change

Move mirroring off the attach primitive and onto a multiplexed stream, one per
host rather than one per pane.

- **One connection per host.** A single `herdr` process on the far side,
  reached over one ssh, streams frames for a *set* of terminals. Frames carry
  their terminal id so one connection serves any number of mirrors.
- **Read is observe, not attach.** Watching claims nothing, so any number of
  machines can mirror the same host and the star stops being mandatory.
- **Write is claimed on focus.** `ControlTerminal { target, takeover }` already
  exists. Claim when the user focuses a mirrored pane, release when they leave.
  One round trip on a connection that is already open, so no switch latency.

Per host, regardless of pane count: 1 ssh, 1 remote process, 1 connection,
1 poller thread. lute goes from ~32 held attaches to 3.

## 4. Protocol

Additive, `PROTOCOL_VERSION` 18 -> 19. As built:

- `ObserveTerminals { targets: Vec<ObservedTarget> }` -- subscribe a connection
  to many terminals at once; the set replaces whatever the connection was
  observing, so it can be updated over the connection's life. Each target
  carries **its own `cols`/`rows`**: a watcher's panes are rarely all the same
  shape, and the connection has no single screen of its own.
- `ObservedTerminal(ObservedTerminalFrame { terminal_id, target, frame })` --
  frames tagged with the terminal they belong to, and with the target string
  the watcher asked by, so a watcher that asked by agent name keeps no map.
- `ObservedTerminalEnded { terminal_id, target, reason }` -- one terminal going
  no longer takes the connection with it.
- Single-target `ObserveTerminal` is untouched, so an old host still mirrors
  the old way. Version negotiation already covers the fallback; no separate
  capability flag was needed. The fleet runs mixed builds routinely (valkyrie
  and cdl sleep through deploys), so the fallback is not optional.

Observed frames are always terminal-ANSI whatever the connection negotiated
for itself -- there is no single semantic screen to send when you are watching
thirty of them.

## 5. Phases

1. **Protocol + server side.** *Done.* `ObserveTerminals`, terminal-tagged
   frames, per-terminal sizes and baselines. The server renders each observed
   terminal at its own size and diffs it against its own baseline, so one
   connection serves a whole host's panes.
2. **Client side.** *Done, behind `remote.multiplexed_mirrors` (default off).*
   Proven with two watchers mirroring one host at once -- 2 mirrors each, one
   `observe-many` process each, 9 open descriptors per watcher against ~14.5
   per mirrored pane before, and typing on the host showing up in both. Three
   pieces:
   - **A remote end to run.** `terminal session observe` already takes one
     target; the multiplexed sibling takes a *set* on stdin so it can be
     updated as panes come and go on that host, and writes the tagged frames
     it receives straight out. One `ssh host herdr ...` per host, no `-t`: no
     pty is needed when nothing is typing into it.
   - **A local reader per host.** Decodes tagged frames off that pipe and
     routes each to the mirror pane holding that terminal id.
   - **A pane runtime with no child.** Today a mirror pane is a pty running
     ssh, and the terminal parser is fed by that pty. A streamed pane needs the
     same parser fed from the host stream instead -- `test_process_pty_bytes`
     is exactly that path, test-only for now -- with input going back up the
     connection rather than into a local pty, and only once focus has claimed
     control.
3. **Claiming control.** *Done.* Claimed on the first keystroke rather than on
   focus, which turned out better: a mirror that is only being watched holds
   nothing, and focus is not the question -- typing is. One writable connection
   per host, moved from pane to pane, which needed two server guards relaxed:
   an established observer may name a new set, and an established controller
   may name another terminal. Both refusals were fatal to the connection, and
   the first one meant a watcher lost its stream whenever the host gained a
   pane.
4. **Fallback.** *Done.* Decided per host and by evidence rather than by
   version: a connection that carried no frames at all means the host cannot do
   this, and its mirrors go back to an attach per pane. A connection that
   worked and then dropped keeps the shared one. The host's complaint is
   logged, since it is the only place an old build says why.
5. **Backoff.** *Done, and it was trivial as predicted* -- one connection to
   re-establish per host, nothing to respawn per pane. Doubling from 2s, capped
   at 32s, per host, cleared by any frame. It matters more than it looks:
   reconcile runs on every snapshot a host sends, not on a timer, so a host
   that is down was previously dialled as fast as its events arrived.
   `keep_offline_mirrors` now costs nothing to leave on, since an offline host
   holds one dead connection rather than one per pane.

## 6. What phase 1 turned up

- **The render slot is per connection, capacity one**, deliberately: a slow
  client should never build lag, and for one screen the newest frame simply
  replaces the older one. That is wrong the moment a connection carries several
  terminals -- frames for different terminals are not substitutes, so all but
  one terminal per tick would have been dropped, and always the same ones.
  The queue now coalesces **per terminal id** instead of per connection, which
  keeps the no-lag property and loses nothing.
- **A baseline is committed on send, not on encode.** A frame that cannot be
  queued leaves the terminal's encoder uncommitted, so the next tick re-encodes
  the same diff rather than losing it.
- **A new wire variant may only be appended.** The tag is the variant's
  position, so grouping `ObservedTerminal` with the other frame variants
  renumbered every message after it: a build one version apart read `Notify`
  as `Clipboard`. Three integration tests caught it. The protocol version also
  lives hardcoded in four places outside `wire.rs` -- the API ping test, the
  status test, the generated schema artifact, and `tests/support` -- and the
  last one does not fail, it *hangs*: the CLI aborts on the version mismatch
  and the fake server waits for a request that never comes.
- **`raise_server_nofile_limit()` was a no-op on Linux** -- an empty function
  body, while macOS had a real one. That is why lute sat at the 1024 default
  while the fd ceiling was the binding constraint. Now 65536, as on macOS.
- **SIGUSR1 asks a running server for a live handoff**, so a deploy no longer
  needs the SIGTERM path that killed twelve agent processes with their PTYs.
  It works on every server it has been tried on -- fresh, handoff-imported,
  under 240 requests/second of load, with twenty panes -- **except lute's live
  one**, which has the handler installed (`SigCgt` bit 10, the install logged
  with its pid), receives the signal (nothing pending afterwards, delivery
  confirmed from two different senders), and does nothing. Not the binary:
  the same file responds when started fresh on the same machine. Cause
  unknown. The instrumentation to catch it next time is in.
- **lute holds 575 open API connections and serves ~240 requests a second,
  for ever.** That is not the mirrors rendering; it is connection churn, and
  it is the same shape of problem this whole plan is about: one connection
  per pane per poll rather than one per host.

## 7. Risks
- **Mixed builds.** Every path needs the old fallback until the whole fleet is
  current, and two of six machines sleep for days.
- **Handoff.** Mirrors are deliberately absent from snapshots and rebuilt after
  a handoff; the new stream must rebuild the same way.
- **Scrollback and resize** semantics per observed terminal, when several
  watchers disagree about size. Observers should not resize the terminal; only
  the controller may.

## 8. Not doing yet

Backoff on reconnect, and a cap on concurrent attaches. Both make the current
design survive rather than fixing it, and both become much smaller once there is
one connection per host. `manage_ssh_config = true` (OpenSSH ControlMaster) is
worth turning on meanwhile: it collapses per-pane ssh handshakes without any
code change.

## Found in production, 2026-08-18 -- feature turned off again

Enabled across the fleet, then disabled the same afternoon. Two bugs, both in
the shared connection rather than in the idea, and one pre-existing hang that
the shared connection turns from a nuisance into a blackout.

1. **One stale terminal id kills every mirror of that host.** The host resolves
   the whole observe set or none of it:

       mirror stream closed target=lute
         reason="terminal session observe failed: terminal target term_6594f67b02c2e22 not found"

   A terminal that ended between discovery and the request takes all 51 mirrors
   down with it, and because the set is retried unchanged the connection dies
   for ever. An attach per pane degraded one pane at a time. Fix: resolve each
   target on its own, answer the unknown ones with `ObservedTerminalEnded`, and
   serve the rest.

2. **A rebuilt mirror pane stays blank.** The host keeps a baseline per
   observed terminal and sends only what changed. Mirror identity includes the
   host's workspace id, and those change whenever the host is handed off -- so
   every deploy rebuilds the observer's panes, which then receive diffs against
   a baseline they never saw. An idle terminal never repaints, so it is blank
   for ever. Fix: reset the baseline whenever a client names its observe set,
   and re-send the set when a streamed pane is created.

3. **The stream's only heartbeat is discovery.** A wedged discovery channel
   (pandora's, after lute was handed off: the ssh alive and sleeping, the
   poller silent from 12:43:00 onward) means no reconcile, so the stream is
   never respawned and every mirror sits blank. Attaches each reconnected on
   their own. The stream needs a retry that does not depend on discovery.

None of this changes the arithmetic that motivated the work: measured on lute
today, an attach per mirrored pane costs **~24 descriptors** (379 fds at 3
attaches, 1189 at 37), so the old 1024 default was a wall at about **43
mirrored panes** -- and while it ran, pandora carried **52 mirrored panes on 63
descriptors**. The saving is real. The connection is not yet resilient enough
to spend it.
