#!/usr/bin/env python3
"""Tests for tools/tidalscan.py -- synthetic fixtures only.

Real .tidal sources are never read or written here (PLAN header: no tool
reads song files during implementation); the corpus sweep is run by hand
against /home/endo/Studio/Tidaling/v6 once the §7b annotation pass lands.

Run: python3 tools/test_tidalscan.py
"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tidalscan as ts

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")

SONG_BASIC = """\
-- header rule
-----
setcps(120/60/4)

-- intro @8b
do
  resetCycles
  b1 $ s "bd*2"

-- verse
do
  l2 $ sound "super"
  b1 $ s "sn"

do -- bridge @16b
  a1 $ s "fx"

-- tail @32 beats
do
  b1 $ slow $ s "lt"
"""

SONG_LEGACY_TAGS = """\
setcps(140/60/4)
do -- meditation @20
  resetCycles
  b1 $ s "bd"
do -- walk @10
  a1 $ s "fx"
"""

SONG_MULTI_SETCPS = """\
setcps(180/60/4)
do -- a
  resetCycles
  b1 $ s "bd"
-- scratch experiments, never executed:
setcps(86/60/4)
setcps (90/60/4)
do -- b
  b2 $ s "hh"
"""

SONG_SPACE_FORM = """\
setcps (142/60/4)
do
  resetCycles
  b1 $ x
"""

SONG_NO_RESET = """\
setcps(120/60/4)
do -- a
  b1 $ s "bd"
do -- b
  resetCycles
  b1 $ s "sn"
"""

SONG_NO_SETCPS = """\
do -- a
  resetCycles
  b1 $ s "bd"
"""

SONG_NESTED_DO = """\
setcps(90/60/4)
do -- a
  resetCycles
  stack
    [ do
        b1 $ s "bd"
    ]
do -- b
  l1 $ y
"""


def fixture(name, body):
    path = os.path.join(FIXTURES, name)
    os.makedirs(FIXTURES, exist_ok=True)
    with open(path, "w") as fh:
        fh.write(body)
    return path


class ScanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = os.path.join(self.tmp.name, "run")

    def tearDown(self):
        self.tmp.cleanup()

    def scan(self, body, name="song"):
        path = fixture(name, body)
        return ts.scan_file(path, self.out), path

    def test_basic_sections_and_labels(self):
        doc, _ = self.scan(SONG_BASIC, "basic")
        secs = doc["sections"]
        self.assertEqual(len(secs), 4)
        self.assertEqual([s["label"] for s in secs],
                         ["intro", "verse", "bridge", "tail"])
        self.assertEqual(doc["cps"], 0.5)      # 120/60/4 (C8 literal)
        self.assertEqual(doc["beats_per_cycle"], 4)

    def test_explicit_beats_and_defaults(self):
        doc, _ = self.scan(SONG_BASIC, "basic2")
        secs = doc["sections"]
        # R3/R20: annotated durations, cumulative default starts.
        self.assertEqual(secs[0]["duration_beats"], 8)
        self.assertEqual(secs[0]["start_beat"], 0)
        # section-2 (labeled "verse") unannotated -> 4 beats, starts at 8.
        self.assertEqual(secs[1]["duration_beats"], 4)
        self.assertEqual(secs[1]["start_beat"], 8)
        self.assertEqual(secs[2]["start_beat"], 12)
        self.assertEqual(secs[2]["duration_beats"], 16)
        self.assertEqual(secs[3]["start_beat"], 28)
        self.assertEqual(secs[3]["duration_beats"], 32)  # "@32 beats" synonym

    def test_unannotated_identical_to_4b(self):
        # R20: default layout 0,4,8,... identical to an @4b-annotated file.
        doc, _ = self.scan(SONG_BASIC, "u1")
        self.assertEqual([s["start_beat"] for s in doc["sections"]],
                         [0, 8, 12, 28])

    def test_line_range_and_orbits(self):
        doc, _ = self.scan(SONG_BASIC, "lr")
        s0 = doc["sections"][0]
        self.assertLessEqual(s0["line_range"]["start"], s0["line_range"]["end"])
        self.assertIn("b1", s0["orbits"])
        self.assertIn("l2", doc["sections"][1]["orbits"])
        self.assertIn("b1", doc["sections"][1]["orbits"])

    def test_legacy_tags_warn_not_fatal(self):
        # R22: @20 / @10 scanned, warned, no dwell written.
        doc, _ = self.scan(SONG_LEGACY_TAGS, "legacy")
        self.assertEqual(doc["cps"], 140 / 60 / 4)
        labels = [s["label"] for s in doc["sections"]]
        self.assertEqual(labels, ["meditation", "walk"])
        for s in doc["sections"]:
            self.assertNotIn("dwell", s)

    def test_multi_setcps_first_wins(self):
        # R19/C11: first wins, later warned, no per-section rate.
        doc, _ = self.scan(SONG_MULTI_SETCPS, "multi")
        self.assertEqual(doc["cps"], 180 / 60 / 4)
        self.assertEqual(doc["beats_per_cycle"], 4)
        for s in doc["sections"]:
            self.assertNotIn("cps", s)

    def test_space_form_parses_identically(self):
        doc, _ = self.scan(SONG_SPACE_FORM, "space")
        self.assertEqual(doc["cps"], 142 / 60 / 4)
        self.assertEqual(doc["beats_per_cycle"], 4)

    def test_missing_reset_cycles_is_fatal(self):
        # R21: resetCycles present but not in section 1.
        with self.assertRaises(ts.ScanError) as cm:
            self.scan(SONG_NO_RESET, "noreset")
        self.assertIn("R21", str(cm.exception))
        self.assertFalse(os.path.exists(os.path.join(self.out, "sections.json")))

    def test_missing_setcps_is_fatal(self):
        # R19: no silent fallback.
        with self.assertRaises(ts.ScanError) as cm:
            self.scan(SONG_NO_SETCPS, "notempo")
        self.assertIn("R19", str(cm.exception))

    def test_nested_do_is_warning_not_section(self):
        # C1/R6: indented do stays a non-section.
        doc, _ = self.scan(SONG_NESTED_DO, "nested")
        self.assertEqual(len(doc["sections"]), 2)

    def test_malformed_annotation_is_fatal(self):
        for bad in ["do -- x @", "do -- x @1.5", "do -- x @90s", "do -- x @16c"]:
            body = "setcps(120/60/4)\n" + bad + "\n  resetCycles\n  b1 $ s \"bd\"\n"
            with self.assertRaises(ts.ScanError, msg=bad):
                self.scan(body, "malformed")

    def test_round_trip_invariant(self):
        # §4: parse(save(document)) == document.
        doc, _ = self.scan(SONG_BASIC, "rt")
        with open(os.path.join(self.out, "sections.json")) as fh:
            self.assertEqual(json.load(fh), doc)

    def test_emit_crosscheck_artifact(self):
        # WP-A §4.3: the Rust reader must agree with this writer.
        # Leave a scanned document under tools/fixtures/ for the
        # round-trip test in src/sections.rs (skipped there when
        # absent, asserted when present).
        path = fixture("basic", SONG_BASIC)
        doc = ts.scan_file(path, os.path.join(FIXTURES, "crosscheck"))
        with open(os.path.join(FIXTURES, "crosscheck", "sections.json")) as fh:
            self.assertEqual(json.load(fh), doc)

    def test_manual_beats_survive_rescan(self):
        # R1/R2: merge by (index, label).
        doc, _ = self.scan(SONG_BASIC, "merge")
        path = os.path.join(self.out, "sections.json")
        with open(path) as fh:
            stored = json.load(fh)
        stored["sections"][1]["start_beat"] = 99
        stored["sections"][1]["duration_beats"] = 20
        with open(path, "w") as fh:
            json.dump(stored, fh)
        again = ts.scan_file(fixture("merge", SONG_BASIC), self.out)
        self.assertEqual(again["sections"][1]["start_beat"], 99)
        self.assertEqual(again["sections"][1]["duration_beats"], 20)

    def test_manual_beats_dropped_on_identity_mismatch(self):
        doc, _ = self.scan(SONG_BASIC, "mismatch")
        path = os.path.join(self.out, "sections.json")
        with open(path) as fh:
            stored = json.load(fh)
        stored["sections"][1]["label"] = "totally different"
        stored["sections"][1]["start_beat"] = 777
        with open(path, "w") as fh:
            json.dump(stored, fh)
        again = ts.scan_file(fixture("mismatch", SONG_BASIC), self.out)
        # index matches, label does not -> scanner value wins, no guessing.
        self.assertEqual(again["sections"][1]["start_beat"], 8)

    def test_legacy_file_does_not_poison_merge(self):
        # A songrun.py .sections.json has no beats -> nothing merged (R2).
        os.makedirs(self.out, exist_ok=True)
        legacy = {"song": "/x", "floor_synths": 3, "sections":
                  [{"label": "intro", "dwell": None, "source": "do",
                    "line": 1, "at": 1000.0}]}
        with open(os.path.join(self.out, ".sections.json"), "w") as fh:
            json.dump(legacy, fh)
        doc = ts.scan_file(fixture("legacymerge", SONG_BASIC), self.out)
        self.assertEqual(doc["sections"][0]["start_beat"], 0)
        self.assertTrue(os.path.exists(os.path.join(self.out, "sections.json")))
        self.assertTrue(os.path.exists(os.path.join(self.out, ".sections.json")))

    def test_scratch_skipped_unless_forced(self):
        # C13: predicate only -- no .tidal files are ever created here.
        self.assertTrue(ts.is_scratch("XX-safespace-90bpm.old.tidal"))
        self.assertTrue(ts.is_scratch("/any/dir/XX-something.old.tidal"))
        self.assertFalse(ts.is_scratch("01-exxo-180bpm.tidal"))
        # scan_file short-circuits before reading a scratch name:
        self.assertIsNone(ts.scan_file("/nonexistent/XX-x.old.tidal", self.out))


if __name__ == "__main__":
    unittest.main(verbosity=2)
