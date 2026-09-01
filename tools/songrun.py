#!/usr/bin/env python3
"""Play a song file section by section and profile it, unattended.

A song file is a sequence of `do` blocks, one per section. This fires each in
turn into a live ghci, marks the transition so cost can be attributed to it,
and runs tools/orbitprof.py alongside. Feed the result to tools/orbitreport.py
for a per-section verdict.

A section is a `do` at **column 0**. Indented `do` belongs to an enclosing
expression, so it is skipped and reported rather than treated as a section.

Labels come from either shape:

    -- omminous
    do
      ...

    do -- omminous
      ...

A line of five or more dashes at column 0, *after* the first section, ends the
song; anything past it is ignored. The column and ordering both matter, because
songs open with indented dash rules as a header.

Append `@SECONDS` to a label to override the dwell for that section:

    -- deep beat @90

Each block is sent wrapped in `:{ ... :}`. ghci interprets one line at a time,
so an unwrapped multi-line `do` silently runs only its first line.

Draining matters more than it looks. A server holding thousands of voices
takes minutes to clear, because voices run for their sample's length rather
than their envelope's -- so sleeping a fixed interval before starting leaves
the first section's numbers contaminated by the previous run. This polls
/status until the synth count stops moving instead of guessing.

Usage:
    tools/songrun.py song4.tidal --ghci-pid 97984
    tools/songrun.py song4.tidal --dwell 90 --out runs/song4
"""
import argparse
import json
import os
import re
import socket
import struct
import subprocess
import sys
import time

MAX_SECTION_LINES = 100     # past this, the patch needs splitting, not profiling

# A section is a `do` at column 0. Indented `do` belongs to an expression --
# real songs carry plenty of those inside `stack [...]` and friends, and
# treating them as sections both invents blocks and truncates the real one.
DO_LINE = re.compile(r"^()do\b(.*)$")

# Five or more dashes alone on a line ends the song. It has to be at column 0
# and after at least one section: songs open with indented dash rules as a
# decorative header, and matching those stops parsing before the first block.
SONG_END = re.compile(r"^-{5,}\s*$")
COMMENT = re.compile(r"^\s*--\s?(.*)$")
DWELL_TAG = re.compile(r"@\s*(\d+(?:\.\d+)?)\s*$")


# ------------------------------------------------------------------- parsing

NESTED_DO = re.compile(r"^\s+do\b")


def parse_song(path):
    """([{label, dwell, source, line}], [nested_do_line_numbers])."""
    lines = open(path).read().split("\n")
    sections, pending_comment, i, n = [], None, 0, len(lines)
    nested = []

    while i < n:
        line = lines[i]
        if sections and SONG_END.match(line):
            break
        if NESTED_DO.match(line):
            nested.append(i + 1)

        m = DO_LINE.match(line)
        if not m:
            c = COMMENT.match(line)
            if c:
                # Remember the most recent comment; it names the next block.
                pending_comment = c.group(1).strip()
            elif line.strip():
                pending_comment = None      # real code broke the association
            i += 1
            continue

        indent, trailing = len(m.group(1)), m.group(2)
        inline = COMMENT.match(trailing.strip()) if "--" in trailing else None
        label = (inline.group(1).strip() if inline
                 else pending_comment or f"section-{len(sections) + 1}")
        pending_comment = None

        # The block runs until a line at or left of `do`'s own indent, ignoring
        # blanks. That handles both column-0 blocks and nested ones.
        body, j = [line], i + 1
        while j < n:
            nxt = lines[j]
            if SONG_END.match(nxt):
                break
            if NESTED_DO.match(nxt):
                nested.append(j + 1)
            if nxt.strip() and (len(nxt) - len(nxt.lstrip())) <= indent:
                break
            body.append(nxt)
            j += 1
        while body and not body[-1].strip():
            body.pop()

        if len(body) > MAX_SECTION_LINES:
            raise SystemExit(
                f"{path}:{i + 1}: section '{label}' is {len(body)} lines "
                f"(limit {MAX_SECTION_LINES}).\n"
                f"Split the patch before profiling it -- a block this long "
                f"cannot be attributed to anything useful."
            )

        dwell = None
        d = DWELL_TAG.search(label)
        if d:
            dwell = float(d.group(1))
            label = DWELL_TAG.sub("", label).strip()

        sections.append({"label": label or f"section-{len(sections) + 1}",
                         "dwell": dwell, "source": "\n".join(body),
                         "line": i + 1})
        i = j

    return sections, nested


# --------------------------------------------------------------------- server

def _pad(b): return b + b"\0" * (4 - len(b) % 4)


def status(port, timeout=0.5):
    """(ugens, synths) from scsynth, or None."""
    try:
        sk = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sk.settimeout(timeout)
        sk.sendto(_pad(b"/status") + _pad(b","), ("127.0.0.1", port))
        d = sk.recv(8192)
        sk.close()
        i = d.index(b"\0"); p = (i // 4 + 1) * 4
        j = d.index(b"\0", p); tags = d[p:j].decode(); p = (j - p) // 4 * 4 + p + 4
        v = []
        for t in tags[1:]:
            if t == "i": v.append(struct.unpack_from(">i", d, p)[0]); p += 4
            elif t == "f": v.append(struct.unpack_from(">f", d, p)[0]); p += 4
            elif t == "d": v.append(struct.unpack_from(">d", d, p)[0]); p += 8
        return v[1], v[2]
    except Exception:
        return None


def wrap(section):
    """ghci evaluates one line at a time, so a multi-line `do` block has to
    arrive inside `:{ ... :}` or only its first line is ever interpreted. The
    label rides along as a comment purely so the app's console log shows which
    section is playing."""
    return f":{{\n-- {section['label']}\n{section['source']}\n:}}"


def send_ghci(pid, text):
    """Write into a running ghci's stdin. The app owns that pipe; opening it
    through /proc gives us a second writer without disturbing the first."""
    fd = os.open(f"/proc/{pid}/fd/0", os.O_WRONLY)
    try:
        os.write(fd, (text.rstrip("\n") + "\n").encode())
    finally:
        os.close(fd)


def drain(pid, port, timeout=180.0, quiet_samples=3):
    """hush, then wait for the synth count to stop falling."""
    send_ghci(pid, "hush")
    t0, last, stable = time.time(), None, 0
    while time.time() - t0 < timeout:
        time.sleep(2.0)
        st = status(port)
        if st is None:
            continue
        _, synths = st
        if synths == last:
            stable += 1
            if stable >= quiet_samples:
                return synths, time.time() - t0
        else:
            stable = 0
        last = synths
    return last, time.time() - t0


# ------------------------------------------------------------------------ run

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("song", help="song file of `do` blocks")
    ap.add_argument("--ghci-pid", type=int, help="target ghci (auto-detected if omitted)")
    ap.add_argument("--port", type=int, default=6110, help="scsynth OSC port")
    ap.add_argument("--node", default="orbits", help="pipewire node name")
    ap.add_argument("--dwell", type=float, default=30.0, help="seconds per section")
    ap.add_argument("--lead", type=float, default=6.0, help="idle seconds before section 1")
    ap.add_argument("--tail", type=float, default=25.0, help="seconds to record after hush")
    ap.add_argument("--out", help="output prefix (default: runs/<songname>)")
    ap.add_argument("--dry-run", action="store_true", help="parse and print, play nothing")
    args = ap.parse_args()

    sections, nested = parse_song(args.song)
    if not sections:
        raise SystemExit(
            f"no column-0 `do` blocks found in {args.song}\n"
            f"(sections must start at column 0; indented `do` is treated as "
            f"part of an expression)")

    total = args.lead + sum(s["dwell"] or args.dwell for s in sections) + args.tail
    print(f"  {args.song}: {len(sections)} section(s), ~{total / 60:.1f} min")
    for k, s in enumerate(sections, 1):
        d = s["dwell"] or args.dwell
        print(f"    {k:>2}. {s['label']:<24} line {s['line']:<5} "
              f"{len(s['source'].splitlines()):>3} lines   dwell {d:.0f}s")
    if nested:
        print()
        print(f"  WARNING: {len(nested)} indented `do` on line(s) "
              + ", ".join(str(x) for x in nested[:20])
              + ("..." if len(nested) > 20 else ""))
        print("  These are inside expressions, so they are not sections. If any")
        print("  was meant to be one, move it to column 0.")
    if args.dry_run:
        return 0

    pid = args.ghci_pid
    if pid is None:
        # `pgrep -f` also matches anything carrying the pattern in its own argv
        # (a shell running this command, for one), so filter on comm. Reading
        # /proc can race a process exiting between the two, hence the guard.
        out = subprocess.run(["pgrep", "-f", "ghci-script.*BootTidal"],
                             capture_output=True, text=True).stdout.split()
        for p in out:
            try:
                if open(f"/proc/{p}/comm").read().startswith("ghc"):
                    pid = int(p)
                    break
            except OSError:
                continue
    if pid is None:
        raise SystemExit(
            "no ghci found -- is the app running? pass --ghci-pid to override")
    if not os.path.exists(f"/proc/{pid}/fd/0"):
        raise SystemExit(f"ghci {pid} has no readable stdin")

    prefix = args.out or os.path.join(
        "runs", os.path.splitext(os.path.basename(args.song))[0])
    os.makedirs(os.path.dirname(prefix) or ".", exist_ok=True)
    marks, jsonl = prefix + ".marks", prefix + ".jsonl"
    open(marks, "w").close()

    print(f"\n  draining before start (hush, then wait for a stable synth count)")
    floor, took = drain(pid, args.port)
    print(f"  settled at {floor} synths after {took:.0f}s")

    here = os.path.dirname(os.path.abspath(__file__))
    prof = subprocess.Popen(
        [sys.executable, os.path.join(here, "orbitprof.py"),
         "--port", str(args.port), "--node", args.node,
         "--secs", str(total), "--marks", marks, "--jsonl", jsonl],
        stdout=open(prefix + ".log", "w"), stderr=subprocess.STDOUT)

    fired = []
    try:
        time.sleep(args.lead)
        for s in sections:
            send_ghci(pid, wrap(s))
            with open(marks, "a") as fh:
                fh.write(f"{time.time()}\t{s['label']}\n")
            fired.append({**s, "at": time.time()})
            print(f"    fired {s['label']}", flush=True)
            time.sleep(s["dwell"] or args.dwell)
        send_ghci(pid, "hush")
        with open(marks, "a") as fh:
            fh.write(f"{time.time()}\thush\n")
        time.sleep(args.tail)
    except KeyboardInterrupt:
        print("\n  interrupted -- hushing")
        send_ghci(pid, "hush")
    finally:
        prof.wait(timeout=60)

    with open(prefix + ".sections.json", "w") as fh:
        json.dump({"song": os.path.abspath(args.song), "floor_synths": floor,
                   "sections": fired}, fh, indent=2)

    print(f"\n  wrote {jsonl}")
    print(f"        {prefix}.sections.json")
    print(f"\n  tools/orbitreport.py {jsonl} --sections {prefix}.sections.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
