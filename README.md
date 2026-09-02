# Hub

A Slint/Rust control panel for a headless TidalCycles rig: it boots and
supervises SuperCollider and Tidal, wires JACK, and records per-orbit stems.

The app is deliberately thin. All audio logic lives in `startup.scd`,
`recording.scd` and `BootTidal.hs` — the app only *launches* processes and
*sends them lines of text*. That keeps one source of truth and means every file
still works by hand in the SC IDE or a plain `ghci`.

---

## Layout

| path | role |
|---|---|
| `src/main.rs` | process supervision, JACK wiring, readiness detection, UI callbacks |
| `src/midi.rs` | ALSA MIDI device enumeration (`aconnect`) for the MIDI panel |
| `src/recordings.rs` | past-session browser data (sizes, durations from file size) |
| `ui/app.slint` | window shell: header tabs (GRAPH / SONG / BENCH), footer |
| `ui/theme.slint` | every color, padding and spacing constant; `NodeState` enum |
| `ui/chain.slint` | the pipeline as a horizontal strip of nodes with readiness dots |
| `ui/sidebar.slint` | stack status, quantum, relay port, MIDI, mutes, recording |
| `ui/timeline.slint` | read-only DAW-style song timeline (mock data until the song runner is embedded) |
| `ui/bench.slint` | benchmark UI shell (not wired yet) |
| `ui/graph.slint.bak` | parked iteration: full 2D node-graph canvas |
| `startup.scd` | SuperCollider boot: server options, SuperDirt, orbits. **CONFIG block at the top** |
| `recording.scd` | multi-track recorder, one stem per orbit |
| `BootTidal.hs` | Tidal boot: ports and the `Tidally` instance |
| `recordings/` | output, one dated folder per session |
| `superdirt/`, `tidal/` | upstream sources, kept for reference while debugging |

---

## Port map

Every port is held **out of the SuperCollider/SuperDirt defaults**, so a stray
sclang (an IDE session, another SuperDirt) can never silently take a socket this
stack depends on.

| port | process | set in | must match |
|---|---|---|---|
| 6110 | scsynth (orbits) | `startup.scd` / `main.rs` | Tidal's `oBusPort` |
| 6111 | scsynth (vocals) | `main.rs` | — |
| 6120 | sclang langPort | `sclang -u 6120` (**command line only**) | — |
| 6122 | SuperDirt | `~dirt.start` | Tidal's `oPort` |
| 6130 | Tidal ctrl listener | `cCtrlPort` | external controllers only |
| 6140 | Hub editor relay | `EDITOR_PORT` in `main.rs` | the lite-xl plugin |

### How the pieces actually talk

Tidal has **two** outbound destinations, not one:

- `/dirt/play` → SuperDirt on **6122**
- control-bus writes → **scsynth directly on 6110**, bypassing sclang entirely
  (`oBusPort`, [Target.hs](tidal/src/Sound/Tidal/Stream/Target.hs))

This matters when the two halves live on different machines — see *Remote split*.

---

## Running

```bash
cargo build && ./target/debug/live_audio_control
```

Boot order is **strict**, and the GRAPH view renders it as a node strip —
`sclang → connect → pin threads → boot tidal` — with a blue readiness dot on
each node once its step is confirmed:

1. **SCLANG** — click the node. Ready when sclang prints
   `HUB: scsynth=6110 langPort=6120 dirt=6122 clientID=0 ...`
2. **CONNECT** — click the node; ready when every `jack_connect` succeeds
3. **PIN THREADS** — click the node; ready once `data-loop.0` is pinned
4. **BOOT TIDAL** — ready on `Connected to SuperDirt.`
5. Play

The next step in the chain is highlighted. Booting a fresh sclang demotes
every later node (their work does not survive a stack reboot).

**Any SuperDirt or server restart requires restarting Tidal.** Tidal handshakes
exactly once, caches SuperDirt's control-bus indices, and never asks again
(`checkHandshake` only re-sends while its bus list is empty, and SuperDirt never
announces itself). Reboot the audio side under a live Tidal and it keeps writing
to the dead instance's busses, silently.

`BOOT SCSYNTH` is the alternative path: scsynth alone, no sclang/SuperDirt. Its
flags are then the server's *only* configuration, so they mirror the `s.options`
block in `startup.scd`.

---

## Editor

Hub owns the Tidal process; the editor is a thin client that ships text to it.

Port **6140** takes newline-delimited lines and relays each one verbatim into
ghci's stdin. It never parses Haskell: the lite-xl plugin already emits `:{`,
the block's lines, then `:}`, and ghci does the multi-line accumulation itself.
The RELAY PORT panel in the sidebar shows its state and can START / STOP /
RESTART it (a stop closes the listener; open connections drain out).

The plugin lives at `~/litexl-tidalcycles` and reaches Hub through `nc`, so it
needs no luasocket. `ctrl+shift+return` evaluates the selection;
*Reconnect to Hub* in the context menu reopens the connection after Hub
restarts.

Quick check without an editor:

```bash
printf ':{\nd1 $ s "bd*4"\n:}\n' | nc -q1 127.0.0.1 6140
```

Hub logs every line it receives. That matters because lite-xl's `process:write`
may not flush: if a block's final `:}` never arrives, ghci stays in multi-line
mode and silently folds the *next* block into the previous one. An unbalanced
`:{` in the log is the tell.

### Pattern names

`d1`..`d16` come from `Sound.Tidal.Boot`. On top of those, `BootTidal.hs` defines
orbit-named aliases matching the groups in `startup.scd`:

| alias | orbits |
|---|---|
| `b1`..`b6` | 0–5 (beats) |
| `l1`..`l6` | 6–11 (leads) |
| `a1`..`a6` | 12–17 (ambients) |

They bind with `|<`, so the orbit is a default a pattern can still override with
its own `# orbit n`. Growing to 4 groups of 9 means extending these to match
`~hubGroups` / `~hubPerGroup`.

## Scaling: orbits and groups

`startup.scd` opens with a CONFIG block. Everything downstream is derived —
nothing hardcodes the orbit count or the group letters.

```supercollider
~hubGroups   = [ "b", "l", "a" ];   // add a 4th group here
~hubPerGroup = 6;                   // 9 gives 36 orbits total
```

Derived from those two: total orbits, `numOutputBusChannels` (2× orbits),
`numAudioBusChannels`, the `~dirt.start` bus list, the `~b1../~l1../~a1..`
handles, and the recorder's stem names.

Two values must be kept in step by hand in [`src/main.rs`](src/main.rs):

- **`ORBITS_OUT_CHANNELS`** drives the CONNECT loop and the direct scsynth
  boot's `-o`. It is 36 today (18 stereo orbits); 36 orbits makes it 72.
- **`MUTE_GROUP_LETTERS` / `ORBITS_PER_GROUP`** drive the mute grid in the
  sidebar. They mirror `~hubGroups` / `~hubPerGroup` and the `b1..` aliases
  in `BootTidal.hs`.

---

## Recording

The RECORDING panel shows live state (● REC + elapsed + target folder) driven
by the `HUB REC:` lines sclang prints, and a browser of past sessions: select
a session to list its stems (size and duration derived from file size — stems
are int24 stereo 48 kHz), OPEN hands the folder to the file manager.

**REC START** / **REC STOP**, with a session name field. Output:

```
recordings/<session>_<timestamp>/{b1..b6,l1..l6,a1..a6}.wav
```

One stereo 24-bit/48kHz stem per orbit, named after its Tidal group.
`s.record` would only give a single mixdown of the master bus, so each orbit's
output bus gets its own `DiskOut` synth, ring buffer and file.

Three details that are load-bearing:

- Allocate all ring buffers → `s.sync` → attach all files → `s.sync` → start
  synths. The server must hold the buffer before `/b_write` can attach a file.
- Recorders sit at the **tail** of the default group, so `In.ar(outBus, 2)` sees
  the orbit's audio for the cycle rather than an empty bus.
- Stop waits 0.3 s after freeing the synths before closing buffers, or DiskOut
  loses its last block.

---

## Quantum

The QUANTUM panel picks the PipeWire quantum: 32…2048, default 512. APPLY
runs `pw-metadata -n settings 0 clock.force-quantum <size>`, which moves the
global graph immediately, and stores the size for the next scsynth boot: the
direct boot's `-p`, `-z` and `-Z` follow it. That matters because
`pw-jack -p` sets `node.force-quantum`, which **overrides** the global
setting — a running server keeps its old quantum until rebooted. The measured
cost/budget numbers are quantum-specific; see `tools/README.md`.

---

## Remote split (Tidal on the laptop, this stack on the headless box)

sclang and scsynth are always co-located — sclang boots scsynth as a child — so
sclang always reaches it over loopback. What changes for a remote Tidal is who
*else* may reach them, and that is **two independent settings**:

```supercollider
~hubScsynthBind = "127.0.0.1";            // -> "0.0.0.0"
~hubTidalSender = NetAddr("127.0.0.1");   // -> NetAddr("<laptop-ip>"), or nil for any
```

plus `oAddress` in `BootTidal.hs`.

Both are needed because Tidal writes control busses straight to scsynth while
`/dirt/play` goes to SuperDirt. Change only one and you get **total silence with
a perfectly healthy-looking log on both machines**.

`jack_connect` (the CONNECT button) must run on whichever machine JACK is on.

---

## Hard-won gotchas

Each of these cost real debugging time and is commented at the relevant line.

**Never pin sclang to a CPU core with realtime priority.** A child inherits both
affinity and scheduling policy, so `taskset -c 4 chrt -f 80 … sclang` put sclang
*and* scsynth on one core at SCHED_FIFO 80 — where an equal-priority thread never
preempts a running one. While sclang fired ~2500 `/b_allocRead` messages during
sample loading, scsynth could not be scheduled to drain its UDP socket and the
kernel discarded the overflow, along with `initTree`'s `/g_new`. The result was
an empty node tree and empty sample buffers, with **no error anywhere**: endless
`FAILURE IN SERVER /n_set Node 1000 not found` and `Buffer UGen: no buffer data`.
`startup.scd` pins scsynth alone via `Server.program`.

**sclang given a file argument never reads stdin.** `sclang foo.scd` runs the
file and ignores stdin forever — the documented trailing `-` does not help. The
app therefore launches bare `sclang -u 6120` and sends `"…/startup.scd".load;`
*down stdin*, so one channel both boots the engine and accepts later commands.

**`Required NNN MB of memory` proves nothing.** SuperDirt computes it from file
headers read language-side; it prints identically when every `/b_allocRead` was
dropped. To check what the server actually holds, ask the server:
`~dirt.soundLibrary.buffers.at('bd')[0].query;`

**`s.options` only apply at boot.** They are inert on an already-running server,
and options set *inside* a `s.reboot` callback take effect on the *following*
boot. `startup.scd` therefore builds a fresh `Server` object that has never
booted and uses `waitForBoot`.

**`scsynth -l 3` reports back as `maxLogins 4`.** scsynth rounds up to a power of
two; sclang adapts and says so. Not a bug.

**Tidal ignores `&serverHostname`/`&serverPort` from the handshake.** It reads
only `&controlBusIndices` and uses its own hardcoded `oBusPort` — so SuperDirt
telling Tidal where the server lives has no effect.

---

## Diagnostics

**DUMP TREE** (`s.queryAllNodes`) is the fastest way to tell a real problem from
a phantom one. A healthy tree is group 1 → group 2 → one group per orbit, each
holding `dirt_monitor2 / rms2 / leslie2 / reverb2 / delay2`. An empty
`NODE TREE Group 0` means the server never received the tree.

**INIT TREE** (`s.initTree`) rebuilds the default group and re-runs `ServerTree`.
Needing it means something ate the boot-time messages.

The **Lates** counter tallies lines containing the word `late`, matched
whole-word and case-insensitively.

The app streams everything both children print to stdout, so run it from a
terminal you can read.
