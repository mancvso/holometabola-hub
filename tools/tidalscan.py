#!/usr/bin/env python3
"""Scan a Tidal song file into the schema-v2 sections file the app edits.

Extracted from tools/songrun.py (which mixed parsing with ghci driving and
subprocess orchestration, and is deprecated). This is the Phase-1 scanner:
source -> runs/<folder>/sections.json, validation, and merge of manual beats
across a re-scan. It never touches the .tidal sources beyond reading the one
named on the command line, and it is the only writer of sections.json while
songrun.py still owns .sections.json (R12/C2: one writer per path).

Grammar (frozen by R6/C1/C3/R19-R22):
  * A section is a `do` at column 0, ended by a dedent to <= its indent.
    An indented `do` is part of an expression: warned about, never a section.
  * `^-{5,}$` at column 0 after the first section ends the song.
  * Label: the most recent preceding comment, the trailing `-- x` on the do
    line, or the synthesized `section-N` (R20).
  * Beat annotation `@Nb` (N integer, b mandatory) on the label line, e.g.
    `-- deep beat @16b`, `do -- @16b`, or the synonym `-- 16 beats`.
    Unannotated duration defaults to 4 beats (R20); start defaults to the
    cumulative sum of previous durations (R3).
  * Legacy unitless `@N` (seconds) tags are ignored with a warning (R22);
    a malformed tag (`@`, `@1.5`, `@90s`, `@16c`) is fatal (R9/C3).
  * The FIRST `setcps(bpm/60/bpc)` (or `setcps (bpm/60/bpc)`) in source order
    is authoritative for cps and beats_per_cycle, song-wide (R19/C8: text
    extraction of three numbers, the expression is never evaluated). Later
    setcps lines are scratch: warned, ignored, never written.
  * A live (non-commented) `resetCycles` must exist in the first section
    (R21); later ones warn.

Fatal errors (R9) exit non-zero and name the rule; no file is written.

Usage:
    tools/tidalscan.py <song-file> --out runs/<folder>
    tools/tidalscan.py <song-file>                    # only if --out omitted
                                                       (defaults to runs/<basename>)

The folder mapping is explicit on the command line (C12); scratch sources
(*.old.tidal, XX-*) are skipped in a corpus sweep unless named directly (C13).
"""
import argparse
import json
import os
import re
import sys

SCHEMA = 2
MAX_SECTION_LINES = 100          # R9: past this the patch needs splitting
DEFAULT_DURATION_BEATS = 4       # R20: one cycle at bpc 4

# Contract note on rates (C8/R19/WP-D, frozen as written): cps is extracted
# verbatim from setcps(bpm/60/bpc) as bpm/60/bpc. The derived working rates
# are: beats-per-second = cps * bpc (one beat = 60/bpm seconds) and
# seconds-per-cycle = 1/cps (one cycle = bpc beats = bpc*60/bpm seconds --
# Tidal's actual cycle length). Dwell for a section is
# duration_beats / (cps * bpc) seconds, per the frozen WP-D/R19 formula.

DO_LINE = re.compile(r"^()do\b(.*)$")
NESTED_DO = re.compile(r"^\s+do\b")
SONG_END = re.compile(r"^-{5,}\s*$")
COMMENT = re.compile(r"^\s*--\s?(.*)$")

# setcps with or without a space before the paren (tierra form), three
# integer parameters separated by slashes (C8: extraction, never eval).
SETCPS = re.compile(r"\bsetcps\s*\(\s*(\d+)\s*/\s*(\d+)\s*/\s*(\d+)\s*\)")

# Beat annotation tails (C3).
BEAT_TAG = re.compile(r"@\s*(\d+)\s*b\s*$")
BEATS_SYNONYM = re.compile(r"(\d+)\s+beats?\s*$")
LEGACY_SECONDS_TAG = re.compile(r"@\s*(\d+)\s*$")   # integer, no unit: R22

RESET_CYCLES = re.compile(r"\bresetCycles\b")


class ScanError(Exception):
    """A fatal validation failure (R9); the message names the rule."""


# ------------------------------------------------------------------- parsing

def strip_tag(label):
    """Return (clean_label, beats_or_None, warnings). Raises on malformed."""
    warnings = []
    m = BEAT_TAG.search(label)
    if m:
        return label[: m.start()].strip(), int(m.group(1)), warnings
    m = BEATS_SYNONYM.search(label)
    if m:
        # Only a real annotation if it trails the label (after optional
        # whitespace); `-- 16 beats` per C3. A leading `@` ("@32 beats")
        # is dropped with the number.
        head = label[: m.start()].rstrip()
        if head.endswith("@"):
            head = head[:-1].rstrip()
        return head, int(m.group(1)), warnings
    m = LEGACY_SECONDS_TAG.search(label)
    if m and "@" in label:
        # Unitless @N: legacy seconds (R22) -- dropped with a warning.
        warnings.append(
            f"legacy @SECONDS tag '@{m.group(1)}' ignored (R22); "
            f"replace with @{m.group(1)}b-style beat annotation"
        )
        return label[: m.start()].strip(), None, warnings
    if "@" in label:
        raise ScanError(
            f"malformed beat annotation in label '{label}' (C3/R22): "
            "use '@<integer>b' (e.g. @16b); '@16', '@1.5', '@90s', '@16c' "
            "are not valid"
        )
    return label.strip(), None, warnings


def parse_song(path):
    """(sections, nested_do_lines, setcps_hits, warnings).

    sections: [{label, duration_beats_or_None, start_beat_or_None,
                line_range, source, orbits, has_reset_cycles, annotated}]
    """
    try:
        with open(path) as fh:
            text = fh.read()
    except OSError as e:
        raise ScanError(f"{path}: cannot read source: {e}")
    lines = text.split("\n")
    sections, pending_comment, i, n = [], None, 0, len(lines)
    nested, setcps_hits, warnings = [], [], []

    def route_orbits(body_lines):
        # Mirrors orbits_in_source() in src/sections.rs: `<orbit> $` at
        # statement start. Kept in sync so both writers agree (§4.3).
        out = []
        for line in body_lines:
            t = line.lstrip()
            m = re.match(r"^([abl]\d+)\s*\$", t)
            if m and m.group(1) not in out:
                out.append(m.group(1))
        return out

    while i < n:
        line = lines[i]
        if sections and SONG_END.match(line):
            break
        if NESTED_DO.match(line):
            nested.append(i + 1)
        if m := SETCPS.search(line):
            commented = line.lstrip().startswith("--")
            setcps_hits.append((i + 1, m.groups(), commented))

        m = DO_LINE.match(line)
        if not m:
            if c := COMMENT.match(line):
                pending_comment = c.group(1).strip()
            elif line.strip():
                pending_comment = None
            i += 1
            continue

        trailing = m.group(2)
        inline = COMMENT.match(trailing.strip()) if "--" in trailing else None
        raw_label = (
            inline.group(1).strip() if inline
            else pending_comment or f"section-{len(sections) + 1}"
        )
        pending_comment = None
        label, beats, tag_warnings = strip_tag(raw_label)
        for w in tag_warnings:
            warnings.append(f"{path}:{i + 1}: {w} [label '{raw_label}']")
        if not label:
            label = f"section-{len(sections) + 1}"

        body, j = [line], i + 1
        reset_here = False
        while j < n:
            nxt = lines[j]
            if SONG_END.match(nxt):
                break
            if NESTED_DO.match(nxt):
                nested.append(j + 1)
            if nxt.strip() and (len(nxt) - len(nxt.lstrip())) <= 0:
                break
            if RESET_CYCLES.search(nxt) and not nxt.lstrip().startswith("--"):
                reset_here = True
            body.append(nxt)
            j += 1
        while body and not body[-1].strip():
            body.pop()

        # R6: the do line itself counts; the block is the source.
        if len(body) > MAX_SECTION_LINES:
            raise ScanError(
                f"R9: {path}:{i + 1}: section '{label}' is {len(body)} lines "
                f"(limit {MAX_SECTION_LINES}). Split the patch."
            )

        if beats is None:
            duration = None            # unannotated: R20 default applied later
            annotated = False
        else:
            if beats < 1:
                raise ScanError(
                    f"R10: {path}:{i + 1}: section '{label}' annotated @{beats}b "
                    "but the minimum length is 1 beat"
                )
            duration = beats
            annotated = True

        sections.append({
            "label": label,
            "duration_beats": duration,
            "start_beat": None,   # scanner never owns an explicit start; R3
            "annotated": annotated,
            "line_range": {"start": i + 1, "end": i + len(body)},
            "source": "\n".join(body),
            "orbits": route_orbits(body),
            "has_reset_cycles": reset_here,
        })
        i = j

    if not sections:
        raise ScanError(
            f"no column-0 `do` blocks found in {path} (R6: sections must "
            "start at column 0; indented `do` is part of an expression)"
        )
    if nested:
        warnings.append(
            f"WARNING: {len(nested)} indented `do` line(s) are inside "
            f"expressions, not sections: {', '.join(map(str, nested[:20]))}"
            + ("..." if len(nested) > 20 else "") + " (C1)"
        )
    return sections, nested, setcps_hits, warnings


def extract_tempo(path, setcps_hits, warnings):
    """First-wins per R19/C8. Returns (cps, beats_per_cycle, warnings)."""
    live = [(ln, params) for ln, params, commented in setcps_hits if not commented]
    if not live:
        raise ScanError(
            f"R19: {path}: no live setcps(bpm/60/bpc) found -- a song without "
            "a declared tempo has no file (add setcps to the source; no "
            "silent fallback)"
        )
    ln, (bpm, sixty, bpc) = live[0]
    if int(sixty) != 60:
        raise ScanError(
            f"C8: {path}:{ln}: setcps({bpm}/{sixty}/{bpc}): expected the "
            "bpm/60/bpc form (middle parameter must be 60)"
        )
    cps = float(bpm) / 60.0 / int(bpc)
    for ln2, _ in live[1:]:
        warnings.append(
            f"R19: {path}:{ln2}: later setcps ignored (scratch line; tempo "
            "is song-wide, the first declaration wins)"
        )
    return cps, int(bpc), warnings


def check_reset_cycles(path, sections, warnings):
    """R21: fatal when section 1 carries no live resetCycles."""
    first = sections[0]
    if not first["has_reset_cycles"]:
        raise ScanError(
            f"R21: {path}:{first['line_range']['start']}: the first section "
            "has no live `resetCycles` -- beat zero must exist before the "
            "first fire (see the annotation pass, PLAN §7b)"
        )
    for k, s in enumerate(sections[1:], start=2):
        if s["has_reset_cycles"]:
            warnings.append(
                f"R21: {path}:{s['line_range']['start']}: section {k} also "
                "carries resetCycles (scratch line; if executed at runtime "
                "the app's interceptor re-bases, which is correct)"
            )


# ------------------------------------------------------------------- beats

def compute_beats(sections):
    """Explicit beats per R3/R20: start = cumulative sum of previous
    durations; unannotated duration = DEFAULT_DURATION_BEATS."""
    cursor = 0
    for s in sections:
        if s["start_beat"] is None:
            s["start_beat"] = cursor
        if s["duration_beats"] is None:
            s["duration_beats"] = DEFAULT_DURATION_BEATS
        cursor = s["start_beat"] + s["duration_beats"]


def merge_manual(existing_sections, new_sections):
    """R1/R2: manual beats survive only where BOTH index and label match;
    on mismatch the scanner's values win and the manual beats are dropped."""
    by_index = {
        sec.get("index"): sec
        for sec in (existing_sections or [])
        if isinstance(sec, dict)
    }
    for k, sec in enumerate(new_sections):
        old = by_index.get(k)
        if old is not None and old.get("label") == sec["label"]:
            if isinstance(old.get("start_beat"), int):
                sec["start_beat"] = old["start_beat"]
            if isinstance(old.get("duration_beats"), int) and old["duration_beats"] >= 1:
                sec["duration_beats"] = old["duration_beats"]


# ------------------------------------------------------------------ output

def build_document(path, cps, bpc, sections):
    return {
        "schema": SCHEMA,
        "song": os.path.abspath(path),
        "beats_per_cycle": bpc,
        "cps": cps,
        "sections": [
            {
                "index": k,
                "label": s["label"],
                "start_beat": int(s["start_beat"]),
                "duration_beats": int(s["duration_beats"]),
                "line_range": dict(s["line_range"]),
                "orbits": list(s["orbits"]),
                "source": s["source"],
                "at": None,
            }
            for k, s in enumerate(sections)
        ],
    }


def load_existing(out_dir):
    """Read a previous sections.json (new name preferred, legacy dot name
    accepted during the C2 transition -- same policy as the Rust reader)."""
    for name in ("sections.json", ".sections.json"):
        p = os.path.join(out_dir, name)
        try:
            with open(p) as fh:
                return json.load(fh)
        except (OSError, ValueError):
            continue
    return None


def write_atomic(doc, out_dir):
    """R12: tmp + rename, never a truncated file at the final path."""
    os.makedirs(out_dir, exist_ok=True)
    target = os.path.join(out_dir, "sections.json")
    tmp = target + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, target)
    return target


# -------------------------------------------------------------------- main

def is_scratch(path):
    base = os.path.basename(path)
    return base.endswith(".old.tidal") or base.startswith("XX-")


def scan_file(path, out_dir, force=False):
    """Scan one source into sections.json under out_dir. Returns the doc."""
    if is_scratch(path) and not force:
        print(f"  skipping scratch source {path} (C13; name it explicitly "
              "with --force to scan it)", file=sys.stderr)
        return None
    sections, _nested, hits, warnings = parse_song(path)
    cps, bpc, warnings = extract_tempo(path, hits, warnings)
    check_reset_cycles(path, sections, warnings)

    existing = load_existing(out_dir)
    # Merge manual beats only across a schema-2 file; a legacy bench file
    # carries no beats, so there is nothing to keep (R2).
    if existing and existing.get("schema") == SCHEMA:
        merge_manual(existing.get("sections"), sections)
    compute_beats(sections)
    doc = build_document(path, cps, bpc, sections)
    target = write_atomic(doc, out_dir)

    # The invariant (§4): read back what we wrote and compare.
    with open(target) as fh:
        if json.load(fh) != doc:
            raise ScanError(f"round-trip check failed for {target}")
    for w in warnings:
        print(f"  WARNING: {w}", file=sys.stderr)
    print(f"  {path}: {len(doc['sections'])} sections, cps={cps}, "
          f"bpc={bpc} -> {target}")
    return doc


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("song", nargs="?", help="song file of `do` blocks")
    ap.add_argument("--out", help="output folder; sections.json is written "
                                  "there (C12: mapping is explicit)")
    ap.add_argument("--force", action="store_true",
                    help="scan scratch sources (*.old.tidal, XX-*) too (C13)")
    args = ap.parse_args()

    if not args.song:
        ap.error("a song file is required")
    out_dir = args.out or os.path.join("runs", os.path.splitext(os.path.basename(args.song))[0])
    try:
        scan_file(args.song, out_dir, force=args.force)
    except ScanError as e:
        print(f"FATAL: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
