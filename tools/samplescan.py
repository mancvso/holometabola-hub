#!/usr/bin/env python3
"""Inventory the custom sample library: duration, rate, and SuperDirt index.

Two things this catches that nothing else will:

  * Long files hiding in a bank of short hits. `exxo:5` is 49s in a bank whose
    other five files average 0.4s -- any orbit that plays it without `sustain`,
    `legato` or a bounded `begin`/`end` holds a node for 49 seconds and
    accumulates until the server dies. That single file caused a 6175-voice
    runaway that read as a scheduling problem for a long time.

  * Files off the server's sample rate. scsynth runs at 48k; a 44.1k buffer
    still plays at the right pitch (SuperDirt applies BufRateScale) but every
    voice pays for the conversion, and `unitDuration` maths gets noisier.

Bank index follows SuperDirt: files sorted by name, so `bank:N` is the Nth
entry, and indices past the end wrap modulo the file count.

Usage:
    tools/samplescan.py                       # human table, flagged rows only
    tools/samplescan.py --all                 # every file
    tools/samplescan.py --json samples.json   # for tools/orbitreport.py
"""
import argparse
import concurrent.futures
import json
import os
import subprocess
import sys

DEFAULT_ROOT = "/home/endo/Studio/Sampling/tidal-samples"
AUDIO_EXT = {".wav", ".aif", ".aiff", ".flac", ".ogg", ".mp3", ".w64", ".caf"}


def probe(path):
    """(duration_s, sample_rate, channels) via ffprobe, or None."""
    try:
        out = subprocess.run(
            ["ffprobe", "-v", "error",
             "-show_entries", "format=duration:stream=sample_rate,channels",
             "-of", "json", path],
            capture_output=True, text=True, timeout=20).stdout
        d = json.loads(out)
        dur = float(d.get("format", {}).get("duration", 0.0) or 0.0)
        st = (d.get("streams") or [{}])[0]
        return dur, int(st.get("sample_rate", 0) or 0), int(st.get("channels", 0) or 0)
    except Exception:
        return None


def scan(root):
    """{bank: [ {index, name, path, dur, rate, chans}, ... ]} in SuperDirt order."""
    banks = {}
    for entry in sorted(os.listdir(root)):
        if entry.startswith("."):
            continue
        bank_dir = os.path.join(root, entry)
        if not os.path.isdir(bank_dir):
            continue
        files = sorted(
            f for f in os.listdir(bank_dir)
            if not f.startswith(".")
            and os.path.splitext(f)[1].lower() in AUDIO_EXT
            and os.path.isfile(os.path.join(bank_dir, f))
        )
        if files:
            banks[entry] = [
                {"index": i, "name": f, "path": os.path.join(bank_dir, f)}
                for i, f in enumerate(files)
            ]

    jobs = [rec for recs in banks.values() for rec in recs]
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        for rec, res in zip(jobs, pool.map(lambda r: probe(r["path"]), jobs)):
            if res:
                rec["dur"], rec["rate"], rec["chans"] = res
            else:
                rec["dur"], rec["rate"], rec["chans"] = None, None, None
    return banks


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--rate", type=int, default=48000, help="server sample rate")
    ap.add_argument("--long", type=float, default=5.0,
                    help="flag files at or above this many seconds")
    ap.add_argument("--all", action="store_true", help="list every file, not just flagged")
    ap.add_argument("--json", help="write the index here, for orbitreport --samples")
    args = ap.parse_args()

    if not os.path.isdir(args.root):
        print(f"no such directory: {args.root}", file=sys.stderr)
        return 1

    banks = scan(args.root)
    if not banks:
        print(f"no audio files under {args.root}", file=sys.stderr)
        return 1

    long_hits, rate_hits, unreadable = [], [], []
    for bank, recs in banks.items():
        durs = [r["dur"] for r in recs if r["dur"]]
        median = sorted(durs)[len(durs) // 2] if durs else 0.0
        for r in recs:
            r["bank"] = bank
            r["ref"] = f"{bank}:{r['index']}"
            if r["dur"] is None:
                unreadable.append(r); continue
            # "Long" in absolute terms, or wildly out of step with its own bank
            # -- the second is what makes a file dangerous, since the patch
            # treats every index in a bank interchangeably.
            r["outlier"] = median > 0 and r["dur"] > max(args.long, median * 8)
            if r["dur"] >= args.long or r["outlier"]:
                long_hits.append(r)
            if r["rate"] and r["rate"] != args.rate:
                rate_hits.append(r)

    total = sum(len(v) for v in banks.values())
    print(f"  {len(banks)} banks, {total} files under {args.root}")
    print()

    if args.all:
        for bank, recs in banks.items():
            print(f"  --- {bank} ({len(recs)}) ---")
            for r in recs:
                d = f"{r['dur']:7.2f}s" if r["dur"] is not None else "      ?"
                rate = f"{r['rate']}" if r["rate"] else "?"
                mark = "  <<<" if r in long_hits else ""
                print(f"    {r['ref']:<14}{d}  {rate:>6}Hz  {r['chans']}ch  {r['name']}{mark}")
        print()

    print(f"  LONG FILES  (>= {args.long}s, or 8x their bank median)")
    if long_hits:
        print(f"    {'ref':<14}{'dur':>9}   file")
        for r in sorted(long_hits, key=lambda x: -x["dur"]):
            why = " (outlier in its bank)" if r.get("outlier") else ""
            print(f"    {r['ref']:<14}{r['dur']:>8.2f}s   {r['bank']}/{r['name']}{why}")
        print()
        print("    These need `sustain`, `legato`, or a bounded `begin`/`end` on any")
        print("    orbit that plays them, or each hit holds a node for its full length.")
    else:
        print("    none")
    print()

    print(f"  OFF-RATE  (server runs at {args.rate})")
    if rate_hits:
        by_rate = {}
        for r in rate_hits:
            by_rate.setdefault(r["rate"], []).append(r)
        for rate, recs in sorted(by_rate.items()):
            print(f"    {rate} Hz -- {len(recs)} file(s): "
                  + ", ".join(sorted({r['bank'] for r in recs})))
    else:
        print("    none -- everything matches")
    if unreadable:
        print()
        print(f"  UNREADABLE: {len(unreadable)}")
        for r in unreadable[:10]:
            print(f"    {r['bank']}/{r['name']}")

    if args.json:
        flat = {}
        for bank, recs in banks.items():
            for r in recs:
                flat[r["ref"]] = {"bank": bank, "index": r["index"], "name": r["name"],
                                  "dur": r["dur"], "rate": r["rate"], "chans": r["chans"],
                                  "count": len(recs)}
        with open(args.json, "w") as fh:
            json.dump({"root": args.root, "server_rate": args.rate,
                       "samples": flat}, fh, indent=2)
        print()
        print(f"  wrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
