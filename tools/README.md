# Hub profiling tools

Four scripts for answering "why did the server choke, and which layer did it."
No dependencies beyond Python 3, `pw-top` and `ffprobe`.

```
samplescan.py  ──json──┐
                       ├──> orbitreport.py  ──> per-section PASS/FAIL + culprit
songrun.py ──> orbitprof.py ──jsonl/marks──┘
```

## The tools

| script | does |
|---|---|
| `orbitprof.py` | samples every orbit's live voice count once a second, plus ugens, xruns and section marks |
| `orbitreport.py` | turns a run into an attribution ranking, a measured ceiling, and a per-section verdict |
| `songrun.py` | plays a song file section by section into a live ghci, marking each change, and runs the profiler alongside |
| `samplescan.py` | inventories the sample library for long files and off-rate files |

## Typical use

```bash
tools/samplescan.py --json samples.json
tools/songrun.py song4.tidal --dwell 90
tools/orbitreport.py runs/song4.jsonl \
    --sections runs/song4.sections.json \
    --samples samples.json
```

`songrun.py --dry-run` parses and prints the section list without playing anything.

## How attribution works

SuperDirt gives every orbit its own scsynth group, allocated in order after the
default group, so **orbit N is group N+3** — `b1..b6` are 3–8, `l1..l6` 9–14,
`a1..a6` 15–20. Polling each group's node tree gives that orbit's live voices.
If `~hubGroups` / `~hubPerGroup` in `startup.scd` change, pass `--groups` and
`--per-group` to match or every number lands on the wrong orbit.

Ranking is by **voice-seconds**, not peak. An orbit spiking to 400 for a second
costs less than one holding 150 for twenty, and only the second kind blows the
budget.

## Measured numbers

At a **512-frame quantum**, on the isolated core:

| | |
|---|---|
| ceiling | xruns become systematic between **25 000 and 30 000 ugens** |
| in voices | ~1 850 synths ≈ **384 concurrent events** |
| cost per synth | ~14–16 ugens |
| synths per event | ~3.3–3.9 (SuperDirt builds one synth per effect control, **per voice**) |
| idle graph | ~4 356 ugens / 90 synths (18 orbits × 5 globalEffects) |

**These are quantum-specific.** At 256 the deadline halves, so the ceiling
roughly halves too — re-measure with a known-heavy song rather than scaling on
faith, and pass the result as `--budget-ugens`.

Reference point: scsynth's DSP is single-threaded, so it is capped at one core
no matter the affinity mask. `supernova` or splitting orbits across servers are
the only ways past that ceiling.

## Traps this exists to catch

**Unbounded voice lifetime.** In `DirtEvent.sc`, if neither `sustain` nor
`legato` is set, sustain falls through to the **whole buffer**. A 49-second
sample then holds a node for 49 seconds per hit. This caused a 6 175-voice
runaway that read as a scheduling problem for a long time. `# sustain N`,
`# legato N` or `# cut N` all bound it — but note `cut` only truncates when the
*next* event arrives, so a pattern going silent leaves the last voice running
full length. `cut` also scans the whole flotsam dictionary per event, so it gets
more expensive exactly when you are in trouble.

**Long files hiding in short banks.** `samplescan.py` flags these:

```
tgl:0    81.68s   (bank median 2.2s)
exxo:5   49.20s   (bank median 0.48s)
```

SuperDirt wraps indices modulo bank size, so with 5 files in `tgl`, `tgl:5`,
`tgl:10` and `tgl:15` all land on the 81-second file.

**Concurrency is rate × lifetime.** Both terms are levers. Shortening voices
removes space; thinning the rate (`striate 4` → `striate 2`, dropping a `layer`
branch) adds it. Which to spend is a musical choice, not a technical one.

## Environment this assumes

- `isolcpus=domain,nohz,managed_irq,4,10` — physical core 4 (CPUs 4 and 10)
  held out of the scheduler. Note the kernel cmdline currently has `isolcpus=`
  **twice**; the kernel honours the first, which omits `managed_irq`, so nvme
  queue IRQs still land on the isolated core. Merge them into one parameter.
- Quantum forced to 512 (`clock.force-quantum`). `pw-jack -p N` sets a
  per-node `node.force-quantum` that **overrides** the global setting, so check
  the client's env, not just `pw-metadata`.
- scsynth at `SCHED_FIFO 83`, sclang at `73`, ghci deliberately **not** realtime
  — a GC pause at realtime priority starves everything below it. Leased from an
  external loop, since the app has no sudo.
- `sched_rt_runtime_us` is 950000, so a FIFO task exceeding 95% of a CPU is
  stopped for 50 ms. Stay clear of that ceiling.
- Wi-Fi off during sessions.
- `irq/128-xhci_hcd` runs `SCHED_FIFO 85` on CPU 10 — the SMT sibling of the
  audio CPU, on the controller the Onyx sits on. Worth moving if headroom gets
  tight.

The app's `PIN THREADS` button splits scsynth's threads: `data-loop.0` (which
does all the DSP) alone on CPU 4, the other seven on CPU 10. Affinity needs no
privileges; the realtime lease does. Press it after the server is booted **and**
connected, since `data-loop.0` only exists once scsynth attaches to PipeWire.
