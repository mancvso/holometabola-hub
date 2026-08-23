#!/usr/bin/env python3
"""Turn an orbitprof.py run into an optimisation target list.

Reads the --jsonl stream from tools/orbitprof.py and derives the things you
actually act on:

  * which orbits owned the render time, ranked by voice-seconds
  * where this machine starts xrunning, in ugens and in voices
  * what a voice costs, and how many synths SuperDirt builds per event
  * which effect parameters are multiplying that per-voice cost

Voice-seconds rather than peak voice count is the ranking that matters: an
orbit spiking to 400 for one second costs less than one holding 150 for
twenty, and only the second kind blows the budget.

Usage:
    tools/orbitreport.py run.jsonl
    tools/orbitreport.py run.jsonl --json      # machine-readable, for the app
"""
import argparse
import collections
import json
import re
import sys

# SuperDirt names its sample players dirt_sample_<in>_<out>. One per event,
# so counting them recovers the event count hiding behind the synth count.
SAMPLER_PREFIX = "dirt_sample_"

# Effect synths SuperDirt adds per voice when the corresponding Tidal control
# is present. Used to explain the per-event synth multiplier back to the user.
EFFECT_CONTROLS = {
    "dirt_lpf2": "cutoff",
    "dirt_hpf2": "hcutoff",
    "dirt_bpf2": "bandf",
    "dirt_fshift2": "fshift",
    "dirt_crush2": "crush",
    "dirt_coarse2": "coarse",
    "dirt_ring2": "ring",
    "dirt_delay2": "delay (global)",
    "dirt_reverb2": "room (global)",
    "dirt_leslie2": "leslie (global)",
    "dirt_envelope2": "attack/release/hold",
    "dirt_gate2": "voice gate (always)",
    "dirt_monitor2": "orbit monitor (global)",
    "dirt_rms2": "orbit rms (global)",
}


def load(path):
    header, samples, footer = None, [], None
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = rec.get("kind")
            if kind == "header":
                header = rec
            elif kind == "sample":
                samples.append(rec)
            elif kind == "footer":
                footer = rec
    return header, samples, footer


def linfit(xs, ys):
    """Least squares y = a + b*x. Returns (a, b) or None when degenerate."""
    n = len(xs)
    if n < 3:
        return None
    sx, sy = sum(xs), sum(ys)
    sxx = sum(x * x for x in xs)
    sxy = sum(x * y for x, y in zip(xs, ys))
    denom = n * sxx - sx * sx
    if denom == 0:
        return None
    b = (n * sxy - sx * sy) / denom
    a = (sy - b * sx) / n
    return a, b


def analyse(header, samples, footer):
    orbits = header["orbits"] if header else sorted(
        {k for s in samples for k in s["orbits"]})

    area = collections.Counter()     # voice-seconds
    peak = collections.Counter()
    defs = collections.Counter()
    overflow_hits = collections.Counter()

    clean_ugens, dirty_ugens = [], []
    clean_voices, dirty_voices = [], []
    ugen_pairs, voice_pairs = [], []
    fit_x, fit_y = [], []
    total_xruns = 0
    elapsed = 0.0

    for s in samples:
        dt = s.get("dt") or 0.0
        elapsed += dt
        counts = s["orbits"]
        total = sum(counts.values())
        for n in orbits:
            c = counts.get(n, 0)
            area[n] += c * dt
            peak[n] = max(peak[n], c)
        for n in s.get("overflow", []):
            overflow_hits[n] += 1
        defs.update(s.get("defs", {}))

        xr = s.get("xruns", 0)
        total_xruns += xr
        ug, sy = s.get("ugens", -1), s.get("synths", -1)
        if ug > 0:
            (dirty_ugens if xr > 0 else clean_ugens).append(ug)
            (dirty_voices if xr > 0 else clean_voices).append(total)
            ugen_pairs.append((ug, xr))
            voice_pairs.append((total, xr))
            if sy > 0:
                fit_x.append(sy); fit_y.append(ug)

    fit = linfit(fit_x, fit_y)

    samplers = sum(c for d, c in defs.items() if d.startswith(SAMPLER_PREFIX))
    per_event = (sum(defs.values()) / samplers) if samplers else None

    return {
        "orbits": orbits,
        "area": area, "peak": peak, "defs": defs,
        "overflow_hits": overflow_hits,
        "elapsed": elapsed,
        "total_xruns": total_xruns,
        "clean_ugens": clean_ugens, "dirty_ugens": dirty_ugens,
        "clean_voices": clean_voices, "dirty_voices": dirty_voices,
        "ugen_pairs": ugen_pairs, "voice_pairs": voice_pairs,
        "dirty_samples": sum(1 for _, x in ugen_pairs if x > 0),
        "fit": fit,
        "samplers": samplers,
        "per_event": per_event,
        "samples": len(samples),
    }


# A ceiling is only real if xruns become the norm above it. The USB interface
# glitches on its own several times a minute regardless of load, so a lone
# xrunning sample says nothing -- taking min(dirty) would report a ceiling far
# below the true one on any clean run.
BIN_UGENS = 2500          # bin width when estimating xrun probability by load
MIN_SUPPORT = 3           # samples needed before a bin gets a vote
DIRTY_RATE = 0.5          # bin counts as "over the edge" past this xrun rate


def threshold(pairs):
    """Load level where xruns stop being incidental and become systematic.

    `pairs` is [(load, xruns), ...] for one metric (ugens or voices). Bins by
    load, finds the lowest bin where most samples xrun, and returns
    (highest_clean_bin_top, that_bin_bottom). None when the run never crosses.
    """
    bins = collections.defaultdict(lambda: [0, 0])   # bin -> [total, dirty]
    for load, xr in pairs:
        if load <= 0:
            continue
        b = int(load // BIN_UGENS)
        bins[b][0] += 1
        if xr > 0:
            bins[b][1] += 1

    ordered = sorted(bins)
    edge = None
    for b in ordered:
        total, dirty = bins[b]
        if total >= MIN_SUPPORT and dirty / total >= DIRTY_RATE:
            edge = b
            break
    if edge is None:
        return None

    clean_top = None
    for b in ordered:
        if b >= edge:
            break
        total, dirty = bins[b]
        if total >= MIN_SUPPORT and dirty / total < DIRTY_RATE:
            clean_top = (b + 1) * BIN_UGENS
    return clean_top, edge * BIN_UGENS


# Constructs that multiply one pattern event into many voices. The count is
# what matters: `striate 4` inside a two-branch `layer` under `jux` is 16x.
MULTIPLIERS = [
    (re.compile(r"\bstriate\s+(\d+)"), "striate {}", lambda m: int(m.group(1))),
    (re.compile(r"\bchop\s+(\d+)"), "chop {}", lambda m: int(m.group(1))),
    (re.compile(r"\bstut\s+(\d+)"), "stut {}", lambda m: int(m.group(1))),
    (re.compile(r"\bjux\b"), "jux", lambda m: 2),
    (re.compile(r"\bsuperimpose\b"), "superimpose", lambda m: 2),
]
# Any of these bounds a voice's lifetime. Without one, sustain falls through to
# the whole buffer -- see DirtEvent.sc, the `unitDuration` branch.
BOUNDS = re.compile(r"#\s*(sustain|legato|cut)\b")
SAMPLE_REF = re.compile(r"\b([a-zA-Z][a-zA-Z0-9_]*):(\d+)")
ORBIT_STMT = re.compile(r"^(\s*)([abl]\d+)\s*\$")


def layer_branches(src):
    """Top-level branch count for each layer[...] / stack[...] in the source."""
    out = []
    for kw in ("layer", "stack"):
        for m in re.finditer(kw + r"\s*\[", src):
            i, depth, commas = m.end(), 1, 0
            while i < len(src) and depth:
                c = src[i]
                if c in "[(": depth += 1
                elif c in ")]": depth -= 1
                elif c == "," and depth == 1: commas += 1
                i += 1
            if commas:
                out.append((kw, commas + 1))
    return out


def orbit_source(section_src, orbit):
    """Just the `<orbit> $ ...` statement out of a section's do block."""
    lines = section_src.split("\n")
    start = None
    for k, ln in enumerate(lines):
        m = ORBIT_STMT.match(ln)
        if m and m.group(2) == orbit:
            start, indent = k, len(m.group(1))
            break
    if start is None:
        return None
    body = [lines[start]]
    for ln in lines[start + 1:]:
        m = ORBIT_STMT.match(ln)
        if m and len(m.group(1)) <= indent:
            break
        body.append(ln)
    return "\n".join(body)


def diagnose(src, sample_index, long_secs=5.0):
    """Why this orbit is expensive, from its source. Ordered most-actionable first."""
    if not src:
        return []
    found = []

    if not BOUNDS.search(src):
        found.append("no `# sustain`, `# legato` or `# cut` -- voices run for the "
                     "whole sample, not the event")

    if sample_index:
        seen = {}
        for m in SAMPLE_REF.finditer(src):
            bank, idx = m.group(1), int(m.group(2))
            rec = sample_index.get(f"{bank}:{idx}")
            if rec is None and bank in {r["bank"] for r in sample_index.values()}:
                count = next(r["count"] for r in sample_index.values() if r["bank"] == bank)
                rec = sample_index.get(f"{bank}:{idx % count}")   # SuperDirt wraps
            if rec and rec.get("dur") and rec["dur"] >= long_secs:
                seen[f"{bank}:{idx}"] = rec["dur"]
        for ref, dur in sorted(seen.items(), key=lambda x: -x[1]):
            found.append(f"plays {ref} ({dur:.1f}s)")

    # Count the constructs, but do not multiply them together: a regex cannot
    # distinguish nesting (which compounds) from slowcat/stack branches (which
    # alternate), and the product of every match is wrong by orders of magnitude.
    mults = collections.Counter()
    for rx, label, _factor in MULTIPLIERS:
        for m in rx.finditer(src):
            mults[label.format(m.group(1) if rx.groups else "")] += 1
    for kw, n in layer_branches(src):
        mults[f"{kw}[{n} branches]"] += 1
    if mults:
        listed = ", ".join(f"{k}" + (f" x{v}" if v > 1 else "")
                           for k, v in mults.most_common())
        found.append(f"event multipliers present: {listed}")
    return found


def sections(samples):
    """Split the run at section marks. Each section gets its own cost and xrun
    tally, so a transition that spikes for three seconds is visible instead of
    being averaged into the steady state around it."""
    segs, cur = [], {"label": "(start)", "t0": 0.0, "samples": []}
    for s in samples:
        for m in s.get("marks", []):
            if cur["samples"]:
                segs.append(cur)
            cur = {"label": m, "t0": s.get("t", 0.0), "samples": []}
        cur["samples"].append(s)
    if cur["samples"]:
        segs.append(cur)

    out = []
    for seg in segs:
        ss = seg["samples"]
        xr = sum(x.get("xruns", 0) for x in ss)
        ug = [x["ugens"] for x in ss if x.get("ugens", 0) > 0]
        vo = [sum(x["orbits"].values()) for x in ss]
        dur = sum(x.get("dt") or 0.0 for x in ss)
        # Peak inside the first few seconds after the mark -- the transition
        # itself, as opposed to wherever the section eventually settles.
        head = ss[:max(1, int(5 / max(0.5, ss[0].get("dt") or 1.0)))]
        per_orbit = collections.Counter()
        for x in ss:
            dt = x.get("dt") or 0.0
            for name, c in x["orbits"].items():
                per_orbit[name] += c * dt
        out.append({
            "orbits": dict(per_orbit.most_common()),
            "label": seg["label"], "t0": seg["t0"], "dur": dur,
            "xruns": xr,
            "peak_ugens": max(ug, default=0),
            "peak_voices": max(vo, default=0),
            "settle_ugens": (sum(ug[len(ug) // 2:]) // max(1, len(ug) - len(ug) // 2))
                            if ug else 0,
            "transient_ugens": max((x["ugens"] for x in head
                                    if x.get("ugens", 0) > 0), default=0),
            "transient_xruns": sum(x.get("xruns", 0) for x in head),
        })
    return out


# Measured on this machine at a 512-frame quantum: xruns become systematic
# somewhere between 25000 and 30000 ugens. At 256 the deadline halves, so this
# has to be re-measured rather than scaled -- pass --budget-ugens.
DEFAULT_BUDGET_UGENS = 25000


def verdict(segs, budget, section_src, sample_index):
    """PASS/FAIL per section, naming the orbit and why it is expensive."""
    print("-" * 74)
    print(f"  BUDGET VERDICT  (ceiling {budget} ugens)")
    print("-" * 74)
    print(f"  {'section':<16}{'peak ug':>9}{'vs budget':>11}{'xruns':>7}  {'verdict':<7} culprit")
    worst = []
    for g in segs:
        if g["label"] in ("(start)", "hush"):
            continue
        ratio = g["peak_ugens"] / budget if budget else 0
        ok = g["peak_ugens"] <= budget and g["xruns"] == 0
        orbits = g.get("orbits") or {}
        top = next((n for n, v in orbits.items() if v > 0), "-")
        print(f"  {g['label'][:15]:<16}{g['peak_ugens']:>9}{ratio:>10.2f}x"
              f"{g['xruns']:>7}  {'PASS' if ok else 'FAIL':<7} {top if not ok else ''}")
        if not ok:
            worst.append((g, top))

    if not worst:
        print()
        print("  all sections inside budget")
        return
    print()
    order = [g["label"] for g in segs]
    for g, top in worst:
        held = (g.get("orbits") or {}).get(top, 0)
        print(f"  {g['label']}: {top} holds {held:.0f} voice-seconds")
        if not section_src:
            print("      - (pass --sections to diagnose the patch source)")
            print()
            continue

        stmt = orbit_source(section_src.get(g["label"], ""), top)
        origin = g["label"]
        if stmt is None:
            # Tidal keeps a pattern running until it is replaced, so the orbit
            # may have been set several sections back. Walk backwards to it.
            for prev in reversed(order[:order.index(g["label"])]):
                stmt = orbit_source(section_src.get(prev, ""), top)
                if stmt is not None:
                    origin = prev
                    break
            if stmt is not None:
                print(f"      - not set here; still running from '{origin}'")
        if stmt is None:
            print(f"      - no `{top} $ ...` statement in this or any earlier section")
        else:
            for w in diagnose(stmt, sample_index):
                print(f"      - {w}")
        print()


def report(a, header, segs=None, budget=None, section_src=None, sample_index=None):
    total_area = sum(a["area"].values()) or 1.0
    print("=" * 74)
    print("  PER-ORBIT RENDER ATTRIBUTION")
    if header:
        print(f"  port {header['port']}  node {header['node']}  "
              f"{a['samples']} samples over {a['elapsed']:.0f}s")
    print("=" * 74)
    print()
    print(f"  {'orbit':<8}{'peak':>7}{'voice-sec':>12}{'share':>8}   bar")
    ranked = [(n, v) for n, v in a["area"].most_common() if v > 0]
    for n, v in ranked:
        pct = v / total_area * 100
        flag = "  (overflowed, counts recovered)" if a["overflow_hits"][n] else ""
        print(f"  {n:<8}{a['peak'][n]:>7}{v:>12.0f}{pct:>7.1f}%   "
              f"{'#' * int(pct / 2)}{flag}")
    if not ranked:
        print("  (no voices recorded)")
    print()

    if ranked:
        top, tv = ranked[0]
        rest = total_area - tv
        print(f"  -> {top} owns {tv / total_area * 100:.1f}% of all render time; "
              f"the other {len(ranked) - 1} orbits together own "
              f"{rest / total_area * 100:.1f}%.")
        print()

    th = threshold(a["ugen_pairs"])
    tv_ = threshold(a["voice_pairs"])
    print("-" * 74)
    print("  BUDGET FOR THIS MACHINE")
    print("-" * 74)
    peak_ug = max(a["clean_ugens"] + a["dirty_ugens"], default=0)
    if th:
        lo = th[0] if th[0] is not None else th[1]
        print(f"  sustained xruns from : {th[1]:>7} ugens"
              + (f"   {tv_[1]:>6} voices" if tv_ else ""))
        print(f"  last consistently ok : {lo:>7} ugens"
              + (f"   {tv_[0]:>6} voices" if tv_ and tv_[0] is not None else ""))
        print(f"  ceiling              : {lo}-{th[1]} ugens")
    else:
        print(f"  no load-related ceiling in this run.")
        print(f"  peak reached         : {peak_ug:>7} ugens with no sustained xruns")
        if a["dirty_samples"]:
            print(f"  ({a['dirty_samples']} isolated xrunning sample(s) out of "
                  f"{a['samples']} -- too sparse to be load-induced; the USB")
            print("   interface glitches on its own regardless of load)")
    print(f"  total xruns          : {a['total_xruns']}")
    print()

    if a["fit"]:
        base, per = a["fit"]
        print(f"  idle graph           : {base:>7.0f} ugens (persistent globalEffects)")
        print(f"  cost per synth       : {per:>7.1f} ugens")
        if a["per_event"]:
            print(f"  synths per event     : {a['per_event']:>7.2f}  "
                  f"(SuperDirt builds one synth per effect control, per voice)")
            print(f"  cost per event       : {per * a['per_event']:>7.1f} ugens")
            if th:
                lo = th[0] if th[0] is not None else th[1]
                # named to avoid shadowing the ugen budget passed in
                event_budget = (lo - base) / (per * a["per_event"])
                print(f"  -> fits about {event_budget:.0f} concurrent events before xruns")
            elif peak_ug:
                head = (peak_ug - base) / (per * a["per_event"])
                print(f"  -> ran {head:.0f} concurrent events cleanly; "
                      f"headroom above this is untested")
    print()

    if segs and len(segs) > 1:
        print("-" * 74)
        print("  BY SECTION  (transient = first 5s after the change)")
        print("-" * 74)
        print(f"  {'section':<14}{'t':>6}{'dur':>6}{'peak ug':>9}{'settle':>8}"
              f"{'peak v':>8}{'xruns':>7}{'of which':>10}")
        print(f"  {'':<14}{'':>6}{'':>6}{'':>9}{'':>8}{'':>8}{'':>7}{'transient':>10}")
        for g in segs:
            print(f"  {g['label'][:13]:<14}{g['t0']:>6.0f}{g['dur']:>6.0f}"
                  f"{g['peak_ugens']:>9}{g['settle_ugens']:>8}"
                  f"{g['peak_voices']:>8}{g['xruns']:>7}{g['transient_xruns']:>10}")
        worst = max(segs, key=lambda g: g["xruns"])
        if worst["xruns"]:
            share = worst["transient_xruns"] / worst["xruns"] * 100
            print()
            print(f"  -> worst section '{worst['label']}': {worst['xruns']} xruns, "
                  f"{share:.0f}% of them in the first 5s after the change")
        print()

    if segs and budget:
        verdict(segs, budget, section_src or {}, sample_index or {})

    print("-" * 74)
    print("  SYNTHDEFS SEEN  (what each voice is actually paying for)")
    print("-" * 74)
    for d, c in a["defs"].most_common(14):
        why = EFFECT_CONTROLS.get(d, "")
        print(f"  {c:>9}  {d:<22}{('<- ' + why) if why else ''}")
    if a["overflow_hits"]:
        print()
        print("  NOTE: synthdef counts exclude orbits whose tree overflowed "
              "(" + ", ".join(sorted(a["overflow_hits"])) + "),")
        print("        so the real mix is weighted further toward those orbits.")
    print()


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("jsonl", help="file written by orbitprof.py --jsonl")
    ap.add_argument("--json", action="store_true", help="emit JSON, for the app")
    ap.add_argument("--sections", help="songrun.py <run>.sections.json, to diagnose the patch")
    ap.add_argument("--samples", help="samplescan.py --json output, to flag long samples")
    ap.add_argument("--budget-ugens", type=int, default=DEFAULT_BUDGET_UGENS,
                    help="ugen ceiling for PASS/FAIL (re-measure per quantum)")
    args = ap.parse_args()

    section_src, sample_index = {}, {}
    if args.sections:
        for sec in json.load(open(args.sections)).get("sections", []):
            section_src[sec["label"]] = sec.get("source", "")
    if args.samples:
        sample_index = json.load(open(args.samples)).get("samples", {})

    header, samples, footer = load(args.jsonl)
    if not samples:
        print(f"no samples in {args.jsonl}", file=sys.stderr)
        return 1
    a = analyse(header, samples, footer)

    if args.json:
        th = threshold(a["ugen_pairs"])
        tv_ = threshold(a["voice_pairs"])
        total_area = sum(a["area"].values()) or 1.0
        json.dump({
            "elapsed": a["elapsed"],
            "samples": a["samples"],
            "total_xruns": a["total_xruns"],
            "attribution": [
                {"orbit": n, "peak": a["peak"][n], "voice_seconds": v,
                 "share": v / total_area,
                 "overflowed": bool(a["overflow_hits"][n])}
                for n, v in a["area"].most_common() if v > 0],
            "threshold_ugens": {"last_clean": th[0], "sustained_xrun": th[1]} if th else None,
            "threshold_voices": {"last_clean": tv_[0], "sustained_xrun": tv_[1]} if tv_ else None,
            "dirty_samples": a["dirty_samples"],
            "peak_ugens": max(a["clean_ugens"] + a["dirty_ugens"], default=0),
            "cost": {"idle_ugens": a["fit"][0], "ugens_per_synth": a["fit"][1]}
                    if a["fit"] else None,
            "synths_per_event": a["per_event"],
            "synthdefs": dict(a["defs"].most_common()),
            "sections": sections(samples),
        }, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        report(a, header, sections(samples), args.budget_ugens,
               section_src, sample_index)
    return 0


if __name__ == "__main__":
    sys.exit(main())
