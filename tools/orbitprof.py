#!/usr/bin/env python3
"""Per-orbit voice profiler for the Hub SuperDirt rig.

Answers "which orbit is costing me the render budget, and when" -- the thing
you cannot get from playing alone, because scsynth only reports totals and
SuperDirt does not attribute them back to orbits.

How the attribution works: SuperDirt gives every orbit its own scsynth group,
allocated in order right after the default group, so orbit N lives in group
N + FIRST_ORBIT_GROUP. Polling each group's node tree gives that orbit's live
voice count. Everything else here is bookkeeping on top of that.

Writes two streams:
  stdout      a human-readable table, one row per sample
  --jsonl     one JSON record per sample, for tools/orbitreport.py

Usage:
    tools/orbitprof.py --port 6110 --node orbits --secs 300 --jsonl run.jsonl
    tools/orbitprof.py --port 57110 --node SuperCollider --secs 120

The layout flags must match startup.scd's ~hubGroups / ~hubPerGroup. If those
change, pass --groups/--per-group or the orbit names will be misaligned and
every number will be attributed to the wrong orbit.
"""
import argparse
import collections
import json
import signal
import socket
import struct
import subprocess
import sys
import threading
import time

# SuperDirt allocates the orbit groups immediately after the default group (1)
# and its own parent group (2), so the first orbit lands on 3.
FIRST_ORBIT_GROUP = 3

# Fallback only. The real value is measured at runtime -- see Profiler.globals.
# SuperDirt's globalEffects list is configurable, so hardcoding it is wrong the
# moment someone adds an effect to the orbit template.
DEFAULT_GLOBAL_FX_PER_ORBIT = 5

# /g_queryTree replies ride a single UDP datagram. scsynth caps that at 64K, so
# a group past roughly this many synths simply never answers. We recover those
# counts from the /status residual instead of dropping the sample.
OVERFLOW_HINT = 2300


# ---------------------------------------------------------------- OSC plumbing

def _pad(b):
    return b + b"\0" * (4 - len(b) % 4)


def _ostr(x):
    return _pad(x.encode())


def osc(addr, *args):
    tags, body = ",", b""
    for a in args:
        if isinstance(a, int):
            tags += "i"; body += struct.pack(">i", a)
        elif isinstance(a, float):
            tags += "f"; body += struct.pack(">f", a)
        else:
            tags += "s"; body += _ostr(a)
    return _ostr(addr) + _ostr(tags) + body


def osc_parse(d):
    i = d.index(b"\0")
    p = (i // 4 + 1) * 4
    j = d.index(b"\0", p)
    tags = d[p:j].decode()
    p = (j - p) // 4 * 4 + p + 4
    out = []
    for t in tags[1:]:
        if t == "i":
            out.append(struct.unpack_from(">i", d, p)[0]); p += 4
        elif t == "f":
            out.append(struct.unpack_from(">f", d, p)[0]); p += 4
        elif t == "d":
            out.append(struct.unpack_from(">d", d, p)[0]); p += 8
        elif t == "s":
            e = d.index(b"\0", p)
            out.append(d[p:e].decode())
            p = (e - p) // 4 * 4 + p + 4
    return out


# ------------------------------------------------------------------- profiling

class Profiler:
    def __init__(self, port, names, timeout=0.5):
        self.port = port
        self.names = names
        self.gid = {n: FIRST_ORBIT_GROUP + i for i, n in enumerate(names)}
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(timeout)
        # Orbit trees get large; a small receive buffer silently truncates them.
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
        # Persistent globalEffect population, measured rather than assumed.
        # The tree walk and the /status read are ~18 round trips apart, so any
        # single residual can be skewed by voices dying in between; keep a
        # window and use the median so one bad sample cannot pin it low.
        self.globals = len(names) * DEFAULT_GLOBAL_FX_PER_ORBIT
        self._residuals = collections.deque(maxlen=32)

    def _send(self, m):
        self.sock.sendto(m, ("127.0.0.1", self.port))

    def group(self, gid):
        """(synth count, synthdef Counter) for one orbit group.

        Returns (None, empty) when the reply overflows the datagram -- the
        caller recovers that orbit's count from the /status residual.
        """
        try:
            self._send(osc("/g_queryTree", gid, 0))
            v = osc_parse(self.sock.recv(1 << 18))
        except Exception:
            return None, collections.Counter()
        if len(v) < 3:
            return None, collections.Counter()

        k = 3  # skip flag, queried node id, child count
        count = 0
        defs = collections.Counter()

        def walk(n):
            nonlocal k, count
            for _ in range(n):
                if k + 1 >= len(v):
                    return
                k += 1              # node id
                children = v[k]; k += 1
                if children == -1:  # synth
                    defs[v[k]] += 1; k += 1
                    count += 1
                else:
                    walk(children)

        walk(v[2])
        return count, defs

    def status(self):
        """(ugens, synths) or (-1, -1)."""
        try:
            self._send(osc("/status"))
            v = osc_parse(self.sock.recv(8192))
            return v[1], v[2]
        except Exception:
            return -1, -1

    def sample(self):
        counts, overflow = {}, []
        defs = collections.Counter()
        for n in self.names:
            c, d = self.group(self.gid[n])
            defs.update(d)
            if c is None:
                overflow.append(n)
                counts[n] = 0
            else:
                counts[n] = c

        ugens, synths = self.status()
        measured = sum(counts.values())

        if not overflow and synths > 0:
            # Nothing overflowed, so anything unaccounted for is global effects.
            # Track the floor: transient event synths only ever push this up.
            seen = synths - measured
            if seen >= 0:
                self._residuals.append(seen)
                ordered = sorted(self._residuals)
                self.globals = ordered[len(ordered) // 2]
        elif overflow and synths > 0:
            # Split the unreadable remainder across the overflowed groups. In
            # practice exactly one orbit overflows at a time, so this is exact.
            residual = synths - self.globals - measured
            if residual > 0:
                share = residual // len(overflow)
                for n in overflow:
                    counts[n] = share
                counts[overflow[-1]] += residual - share * len(overflow)

        return counts, overflow, ugens, synths, defs


# ------------------------------------------------------------ xrun sampling

class XrunPoller(threading.Thread):
    """pw-top costs ~1s per call, so it runs on its own thread and the main
    loop just reads the latest value. Sampling it inline would stretch the
    profiling cadence and smear the timeline."""

    def __init__(self, node, interval=2.0):
        super().__init__(daemon=True)
        self.node = node
        self.interval = interval
        self.value = 0
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            try:
                out = subprocess.run(
                    ["pw-top", "-b", "-n", "2"],
                    capture_output=True, text=True, timeout=8).stdout
                for line in reversed(out.splitlines()):
                    f = line.split()
                    if f and f[-1] == self.node and len(f) > 9:
                        self.value = int(f[8])
                        break
            except Exception:
                pass
            self.stop.wait(self.interval)


# ------------------------------------------------------------------------ main

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=6110, help="scsynth OSC port")
    ap.add_argument("--node", default="orbits", help="pipewire node name, for xruns")
    ap.add_argument("--secs", type=float, default=300.0)
    ap.add_argument("--interval", type=float, default=1.0)
    ap.add_argument("--groups", default="b,l,a", help="matches ~hubGroups")
    ap.add_argument("--per-group", type=int, default=6, help="matches ~hubPerGroup")
    ap.add_argument("--jsonl", help="write one JSON record per sample here")
    ap.add_argument("--marks", help="file the player appends '<unix_ts>\\t<label>' "
                                    "lines to; marks land in the sample stream so "
                                    "cost can be attributed to section changes")
    args = ap.parse_args()

    groups = [g.strip() for g in args.groups.split(",") if g.strip()]
    names = [f"{g}{i + 1}" for g in groups for i in range(args.per_group)]

    prof = Profiler(args.port, names)
    xr = XrunPoller(args.node)
    xr.start()
    time.sleep(min(2.2, xr.interval + 0.2))  # let the first xrun reading land

    out = open(args.jsonl, "w") if args.jsonl else None
    if out:
        json.dump({"kind": "header", "port": args.port, "node": args.node,
                   "groups": groups, "per_group": args.per_group,
                   "orbits": names, "started": time.time(),
                   "interval": args.interval}, out)
        out.write("\n"); out.flush()

    print(f"### PER-ORBIT VOICE PROFILE  port={args.port} node={args.node} ###")
    print(f"{'t':>5} " + "".join(f"{n:>5}" for n in names)
          + f"{'TOT':>7}{'ugens':>8}{'xrun':>6}")
    print("  " + "-" * (5 + 5 * len(names) + 21))

    running = {"v": True}

    def bye(*_):
        running["v"] = False
    signal.signal(signal.SIGINT, bye)
    signal.signal(signal.SIGTERM, bye)

    t0 = time.time()
    last = t0
    xprev = xr.value
    xr0 = xr.value
    marks_seen = 0

    def new_marks():
        """Lines appended to --marks since the last sample. The player writes
        them when it fires a section, so xruns can be tied to the transition
        that caused them rather than to whatever was steady-state at the time."""
        nonlocal marks_seen
        if not args.marks:
            return []
        try:
            with open(args.marks) as fh:
                lines = [l.rstrip("\n") for l in fh if l.strip()]
        except OSError:
            return []
        fresh = lines[marks_seen:]
        marks_seen = len(lines)
        return [l.split("\t", 1)[-1] for l in fresh]

    while running["v"] and time.time() - t0 < args.secs:
        counts, overflow, ugens, synths, defs = prof.sample()
        now = time.time()
        dt = now - last
        last = now
        xruns = xr.value - xprev
        xprev = xr.value
        total = sum(counts.values())

        marks = new_marks()
        for m in marks:
            print(f"  ---- {m} ----", flush=True)

        cells = "".join(
            f"{('~' + str(counts[n])) if n in overflow else str(counts[n]):>5}"
            for n in names)
        print(f"{now - t0:5.0f} {cells}{total:>7}{ugens:>8}{xruns:>6}", flush=True)

        if out:
            json.dump({"kind": "sample", "t": round(now - t0, 3), "wall": now,
                       "dt": round(dt, 3), "orbits": counts,
                       "overflow": overflow, "ugens": ugens, "synths": synths,
                       "xruns": xruns, "defs": dict(defs),
                       "marks": marks}, out)
            out.write("\n"); out.flush()

        time.sleep(max(0.0, args.interval - (time.time() - now)))

    xr.stop.set()
    if out:
        json.dump({"kind": "footer", "ended": time.time(),
                   "elapsed": time.time() - t0,
                   "total_xruns": xr.value - xr0,
                   "globals": prof.globals}, out)
        out.write("\n")
        out.close()
        print(f"\n  wrote {args.jsonl}  --  analyse with tools/orbitreport.py")
    print(f"  total xruns: {xr.value - xr0}   measured globalEffect synths: {prof.globals}")


if __name__ == "__main__":
    sys.exit(main())
