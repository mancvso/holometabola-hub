// ─────────────────────────────────────────────────────────────
// The sections document: runs/<SONG>/sections.json (schema v2) --
// the file the SONG view timeline renders, edits and saves.
//
// Schema v2 (PLAN §5, written by tools/tidalscan.py):
//   {
//     "schema": 2,
//     "song": "/abs/path/song.tidal",
//     "beats_per_cycle": 4,
//     "cps": 0.75,                      // from the FIRST setcps (R19/C8)
//     "sections": [
//       { "index": 0, "label": "deep beat",
//         "start_beat": 32, "duration_beats": 16,
//         "line_range": {"start": 462, "end": 492},
//         "orbits": ["b2", "l2"],
//         "source": "do\n  b2 $ ...",
//         "at": null }
//     ]
//   }
//
// Rate convention (frozen by C8/R19 + WP-D): cps is the literal third
// parameter form from setcps(bpm/60/bpc), i.e. bpm/60/bpc. Therefore
//   beats per second = cps * beats_per_cycle        (bpm / 60)
//   seconds per cycle = 1 / cps                     (bpc * 60 / bpm)
//   dwell seconds     = duration_beats / (cps * bpc)
// Tempo is song-wide; a section carries no rate (R19).
//
// `start_beat` / `duration_beats` are explicit on disk and are NEVER
// recomputed by a read path once present (§4). The only derivation
// left is the legacy fallback: today's bench output (.sections.json,
// wall-clock `at` / `dwell`, no beat fields at all), which loads with
// beats derived at a default tempo purely so the view shows the same
// shape; the user re-scans with tidalscan.py to get real beats.
//
// The file lives inside each song folder under RUNS_DIR. New name is
// `sections.json` (C2); the legacy dot-file `.sections.json` is still
// accepted for one release, the new name winning when both exist --
// songrun.py (deprecated) owns the dot-file, the app owns the plain
// one, one writer per path (R12/guard 21).
//
// JSON is read and written by hand: the crate deps stay slint +
// tokio (serde stays out by WP-A), and the writer must match the
// Python scanner's output so the round-trip invariant holds on both
// sides (§4.3).
// ─────────────────────────────────────────────────────────────

use std::path::{Path, PathBuf};

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{GroupData, SectionData, TrackData};

// Song runs live here, one folder per song. Matches the layout the
// scanner writes (tools/tidalscan.py --out runs/<folder>) and that
// the SONG view dropdown enumerates. Absolute so it works regardless
// of the app's cwd (same convention as RECORDINGS_DIR).
pub const RUNS_DIR: &str = "/home/endo/Studio/Hub/runs";

// App-owned file name (C2) and the legacy dot-file it replaces.
pub const SECTIONS_FILE: &str = "sections.json";
pub const LEGACY_SECTIONS_FILE: &str = ".sections.json";

// Static group definition: display name + orbit prefix. The order
// here is the order the groups appear on the timeline; it matches
// labels::MUTE_GROUP_NAMES.
const GROUPS: [(&str, char); 3] = [
    ("BEATS", 'b'),
    ("LEADS", 'l'),
    ("AMBIENTS", 'a'),
];

// Highest orbit index per group we will materialize as a lane.
const MAX_ORBITS_PER_GROUP: usize = 6;

// Legacy fallback tempo (§4.1): a file with no beat fields and no
// declared rate is displayed at this BPM, 4 beats per cycle, purely
// so the shape renders. Never write these values back as if they
// were scanned: a save of a legacy doc is allowed (the app's own
// file), but the title line warns and the re-scan is the fix.
const LEGACY_DEFAULT_BPM: f64 = 120.0;
const LEGACY_DEFAULT_BPC: i64 = 4;

// ── the document ─────────────────────────────────────────────

/// One section (a column-0 `do` block), placed on the timeline in
/// integer musical beats. `start_beat`/`duration_beats` are
/// authoritative when present; nothing re-derives them (§4).
#[derive(Debug, Clone, PartialEq)]
pub struct DocSection {
    pub index: usize,
    pub label: String,
    pub start_beat: i64,
    pub duration_beats: i64,
    pub line_start: i64,
    pub line_end: i64,
    pub orbits: Vec<String>,
    pub source: String,
    /// Bench telemetry only; never on the app's write path (§5).
    pub at: Option<f64>,
    /// Legacy only; folded into duration_beats at read time.
    pub dwell: Option<f64>,
}

/// The in-Rust document the UI edits (R11): the file's data plus
/// the dirty flag and the path it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SongDoc {
    pub schema: i64,
    pub song: String,
    pub beats_per_cycle: i64,
    pub cps: f64,
    pub sections: Vec<DocSection>,
    /// The file this document was read from (and saves back to).
    pub path: PathBuf,
    pub dirty: bool,
}

impl SongDoc {
    pub fn empty(path: PathBuf) -> Self {
        SongDoc {
            schema: 0,
            song: String::new(),
            beats_per_cycle: LEGACY_DEFAULT_BPC,
            cps: LEGACY_DEFAULT_BPM / 60.0 / LEGACY_DEFAULT_BPC as f64,
            sections: Vec::new(),
            path,
            dirty: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Total song length in beats: the end of the last-ending
    /// section (starts are ordered, but overlap makes ends the
    /// bound -- C4).
    pub fn length_beats(&self) -> i64 {
        self.sections
            .iter()
            .map(|s| s.start_beat + s.duration_beats)
            .max()
            .unwrap_or(0)
    }

    /// Seconds per beat: cps * bpc is the true beats-per-second
    /// under the C8 extraction rule.
    pub fn seconds_per_beat(&self) -> f64 {
        let bps = self.cps * self.beats_per_cycle as f64;
        if bps > 0.0 {
            1.0 / bps
        } else {
            1.0
        }
    }

    // ── editing (R10/C4: legality is enforced here, in Rust) ──

    /// Shift a section's start by `delta` beats. Clamped: never
    /// negative, never before the previous section's start, never
    /// after the next section's start (order preserved, equal is
    /// allowed). Duration is untouched (R16: shift, not stretch).
    /// Returns true when something moved.
    pub fn step_start(&mut self, idx: usize, delta: i64) -> bool {
        let Some(sec) = self.sections.get(idx) else {
            return false;
        };
        let mut next = sec.start_beat.saturating_add(delta);
        if next < 0 {
            next = 0;
        }
        if idx > 0 {
            let prev_start = self.sections[idx - 1].start_beat;
            if next < prev_start {
                next = prev_start; // equal allowed (C4)
            }
        }
        if idx + 1 < self.sections.len() {
            let later = self.sections[idx + 1].start_beat;
            if next > later {
                next = later;
            }
        }
        let sec = &mut self.sections[idx];
        if sec.start_beat == next {
            return false;
        }
        sec.start_beat = next;
        self.dirty = true;
        true
    }

    /// Change a section's length by `delta` beats. Minimum length is
    /// 1 beat (R10); growing past the next start is legal (overlap
    /// is the normal case, C4). Returns true when something moved.
    pub fn step_length(&mut self, idx: usize, delta: i64) -> bool {
        let Some(sec) = self.sections.get_mut(idx) else {
            return false;
        };
        let next = (sec.duration_beats + delta).max(1);
        if sec.duration_beats == next {
            return false;
        }
        sec.duration_beats = next;
        self.dirty = true;
        true
    }
}

// ── orbit routing ────────────────────────────────────────────
// A section routes onto a lane when its `source` contains a
// `<orbit> $` statement at line start (after indentation). Mirrors
// the Python scanner's route_orbits() in tools/tidalscan.py -- both
// sides must agree or the serializers drift apart (§4.3).
fn orbits_in_source(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(|c: char| matches!(c, 'a' | 'b' | 'l')) else {
            continue;
        };
        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits_end == 0 {
            continue;
        }
        let after = &rest[digits_end..];
        let after_trim = after.trim_start();
        if after_trim.starts_with('$') {
            let orbit = format!("{}{}", &trimmed[..1], &rest[..digits_end]);
            if !out.contains(&orbit) {
                out.push(orbit);
            }
        }
    }
    out
}

// ── JSON tokenization (minimal, tolerant) ────────────────────

struct JsonScan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonScan<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Read a JSON string literal or the keyword `null`. Returns
    /// None for null / malformed / wrong cursor. Callers that must
    /// tell "absent" from "null" check the cursor first.
    fn read_string(&mut self) -> Option<String> {
        self.skip_ws();
        if self.peek()? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        while let Some(b) = self.bump() {
            match b {
                b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let esc = self.bump()?;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'u' => {
                            let hex =
                                std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?).ok()?;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            if let Some(ch) = char::from_u32(cp) {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                        _ => out.push(esc),
                    }
                }
                _ => out.push(b),
            }
        }
        None
    }

    fn read_number(&mut self) -> Option<f64> {
        self.skip_ws();
        let start = self.pos;
        if let Some(b'-') = self.peek() {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start || (self.pos == start + 1 && self.bytes.get(start) == Some(&b'-')) {
            return None;
        }
        std::str::from_utf8(self.bytes.get(start..self.pos)?).ok()?.parse::<f64>().ok()
    }

    /// Skip the keyword `null` if present. Returns true when it was.
    fn read_null(&mut self) -> bool {
        self.skip_ws();
        if self.peek() != Some(b'n') {
            return false;
        }
        let before = self.pos;
        for expect in [b'n', b'u', b'l', b'l'] {
            if self.peek() == Some(expect) {
                self.pos += 1;
            } else {
                self.pos = before;
                return false;
            }
        }
        true
    }

    fn skip_value(&mut self) {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                _ = self.read_string();
            }
            Some(b'{') => self.skip_braced(b'{', b'}'),
            Some(b'[') => self.skip_braced(b'[', b']'),
            Some(b't') | Some(b'f') | Some(b'n') => {
                while let Some(b) = self.peek() {
                    if b.is_ascii_alphabetic() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            Some(_) => {
                _ = self.read_number();
            }
            None => {}
        }
    }

    fn skip_braced(&mut self, open: u8, close: u8) {
        let mut depth = 0i32;
        while let Some(b) = self.bump() {
            if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if b == b'"' {
                _ = self.read_string();
            }
        }
    }

    /// Read a flat array of strings (`"orbits": ["b2", "l2"]`).
    fn read_string_array(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() != Some(b'[') {
            return out;
        }
        self.pos += 1;
        loop {
            self.skip_ws();
            match self.peek() {
                None => return out,
                Some(b']') => {
                    self.pos += 1;
                    return out;
                }
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'"') => {
                    if let Some(s) = self.read_string() {
                        out.push(s);
                    } else {
                        return out;
                    }
                }
                _ => {
                    self.skip_value();
                }
            }
        }
    }
}

/// One section object, both schemas. Unknown keys skipped; a
/// section that never closes is dropped (tolerant of truncation --
/// a half-written file reads as "what closed cleanly", never a
/// crash).
fn read_one_section(scan: &mut JsonScan<'_>) -> Option<DocSection> {
    scan.skip_ws();
    if scan.peek()? != b'{' {
        return None;
    }
    scan.pos += 1;
    let mut label = None;
    let mut index: Option<i64> = None;
    let mut start_beat: Option<i64> = None;
    let mut duration_beats: Option<i64> = None;
    let mut line_start: Option<i64> = None;
    let mut line_end: Option<i64> = None;
    let mut orbits: Vec<String> = Vec::new();
    let mut source = None;
    let mut at = None;
    let mut dwell = None;
    loop {
        scan.skip_ws();
        match scan.peek() {
            None => return None,
            Some(b'}') => {
                scan.pos += 1;
                break;
            }
            Some(b',') => {
                scan.pos += 1;
            }
            Some(b'"') => {
                let key = scan.read_string()?;
                scan.skip_ws();
                if scan.peek() == Some(b':') {
                    scan.pos += 1;
                }
                match key.as_str() {
                    "label" => label = scan.read_string(),
                    "source" => source = scan.read_string(),
                    "index" => index = scan.read_number().map(|n| n as i64),
                    "start_beat" => start_beat = scan.read_number().map(|n| n as i64),
                    "duration_beats" => duration_beats = scan.read_number().map(|n| n as i64),
                    "line_range" => {
                        // {"start": N, "end": M}
                        scan.skip_ws();
                        if scan.peek() == Some(b'{') {
                            scan.pos += 1;
                            loop {
                                scan.skip_ws();
                                match scan.peek() {
                                    None | Some(b'}') => {
                                        if scan.peek() == Some(b'}') {
                                            scan.pos += 1;
                                        }
                                        break;
                                    }
                                    Some(b',') => scan.pos += 1,
                                    Some(b'"') => {
                                        let k = scan.read_string()?;
                                        scan.skip_ws();
                                        if scan.peek() == Some(b':') {
                                            scan.pos += 1;
                                        }
                                        let v = scan.read_number().map(|n| n as i64);
                                        match k.as_str() {
                                            "start" => line_start = v,
                                            "end" => line_end = v,
                                            _ => {}
                                        }
                                    }
                                    _ => scan.pos += 1,
                                }
                            }
                        }
                    }
                    "orbits" => orbits = scan.read_string_array(),
                    "at" => {
                        if !scan.read_null() {
                            at = scan.read_number();
                        }
                    }
                    "dwell" => {
                        if !scan.read_null() {
                            dwell = scan.read_number();
                        }
                    }
                    "line" => {
                        // legacy: line is the start line number
                        if line_start.is_none() {
                            line_start = scan.read_number().map(|n| n as i64);
                        }
                    }
                    _ => scan.skip_value(),
                }
            }
            _ => {
                scan.pos += 1;
            }
        }
    }
    // A section needs at least a placement: explicit beats (v2) or an
    // `at` stamp (legacy). A v2 section without beats and without at
    // cannot be placed -> dropped with the file left alone.
    if start_beat.is_none() && at.is_none() {
        return None;
    }
    Some(DocSection {
        index: index.unwrap_or(0) as usize,
        label: label.unwrap_or_default(),
        start_beat: start_beat.unwrap_or(i64::MIN), // sentinel: derive me
        duration_beats: duration_beats.unwrap_or(0),
        line_start: line_start.unwrap_or(0),
        line_end: line_end.unwrap_or(0),
        orbits,
        source: source.unwrap_or_default(),
        at,
        dwell,
    })
}

fn read_sections_array(scan: &mut JsonScan<'_>) -> Vec<DocSection> {
    let mut out = Vec::new();
    scan.skip_ws();
    if scan.peek() != Some(b'[') {
        return out;
    }
    scan.pos += 1;
    loop {
        scan.skip_ws();
        match scan.peek() {
            None => return out,
            Some(b']') => {
                scan.pos += 1;
                return out;
            }
            Some(b',') => {
                scan.pos += 1;
            }
            Some(b'{') => {
                if let Some(sec) = read_one_section(scan) {
                    out.push(sec);
                } else {
                    // object that failed to parse: skip it whole so
                    // the rest of the array is still reachable.
                    scan.skip_value();
                }
            }
            _ => {
                scan.pos += 1;
            }
        }
    }
}

/// Parse a sections document (either schema) from text. `path` is
/// recorded for the save path. Never fails destructively: an
/// unparseable document yields an empty SongDoc (schema 0) so the
/// caller can distinguish "nothing here" from a real file.
pub fn parse_sections_json(text: &str, path: &Path) -> (SongDoc, Vec<String>) {
    let mut warnings = Vec::new();
    let mut scan = JsonScan::new(text);
    let mut doc = SongDoc {
        schema: 0,
        song: String::new(),
        beats_per_cycle: LEGACY_DEFAULT_BPC,
        cps: LEGACY_DEFAULT_BPM / 60.0 / LEGACY_DEFAULT_BPC as f64,
        sections: Vec::new(),
        path: path.to_path_buf(),
        dirty: false,
    };

    scan.skip_ws();
    if scan.peek() != Some(b'{') {
        return (doc, warnings);
    }
    scan.pos += 1;

    loop {
        scan.skip_ws();
        match scan.peek() {
            None => break,
            Some(b'}') => {
                let _ = scan.bump();
                break;
            }
            Some(b',') => {
                scan.pos += 1;
            }
            Some(b'"') => {
                let key = scan.read_string().unwrap_or_default();
                scan.skip_ws();
                if scan.peek() == Some(b':') {
                    scan.pos += 1;
                }
                match key.as_str() {
                    "song" => doc.song = scan.read_string().unwrap_or_default(),
                    "schema" => {
                        if let Some(n) = scan.read_number() {
                            doc.schema = n as i64;
                        }
                    }
                    "cps" => {
                        if let Some(n) = scan.read_number() {
                            doc.cps = n;
                        }
                    }
                    "beats_per_cycle" => {
                        if let Some(n) = scan.read_number() {
                            doc.beats_per_cycle = n as i64;
                        }
                    }
                    "sections" => doc.sections = read_sections_array(&mut scan),
                    _ => scan.skip_value(),
                }
            }
            _ => {
                scan.pos += 1;
            }
        }
    }

    if doc.beats_per_cycle < 1 {
        doc.beats_per_cycle = LEGACY_DEFAULT_BPC;
    }

    // Are the beats explicit on every surviving section? read_one_section
    // already dropped unplaceable ones; those with the sentinel need the
    // legacy derivation.
    let legacy = doc.sections.iter().any(|s| s.start_beat == i64::MIN);
    if legacy {
        warnings.push(
            "legacy bench file: beats derived from wall-clock at/ dwell; \
             re-scan with tools/tidalscan.py for real beats"
                .to_string(),
        );
        derive_legacy(&mut doc);
    } else {
        // v2: explicit beats are law (§4.1) -- the only touch is
        // identity repair when `index` was missing, and the floor to
        // R10's minimums.
        for (k, s) in doc.sections.iter_mut().enumerate() {
            if s.index == 0 && k != 0 {
                s.index = k; // tolerate a hand-written file without indices
            }
            if s.duration_beats < 1 {
                s.duration_beats = 1;
                warnings.push(format!(
                    "section {} ('{}') had duration < 1 beat; floored to 1 (R10)",
                    s.index, s.label
                ));
            }
            if s.start_beat < 0 {
                s.start_beat = 0;
                warnings.push(format!("section {} had a negative start; clamped (R10)", s.index));
            }
        }
    }

    // Orbits: prefer the explicit field (v2); route from source when
    // absent (legacy) so both file shapes render the same lanes.
    for s in &mut doc.sections {
        if s.orbits.is_empty() && !s.source.is_empty() {
            s.orbits = orbits_in_source(&s.source);
        }
        if s.label.is_empty() {
            s.label = format!("section-{}", s.index + 1);
        }
    }

    (doc, warnings)
}

/// Legacy derivation: sort by `at` (stable -- equal stamps keep
/// source order, guard 7), offset from the first, duration from
/// dwell / gap / default, then project seconds onto beats at the
/// fallback tempo. Used ONLY on files with no beat fields at all.
fn derive_legacy(doc: &mut SongDoc) {
    doc.schema = 1;
    doc.beats_per_cycle = LEGACY_DEFAULT_BPC;
    doc.cps = LEGACY_DEFAULT_BPM / 60.0 / LEGACY_DEFAULT_BPC as f64;
    doc.sections.sort_by(|a, b| {
        // Sections with no `at` sort last (they are dropped next).
        let ka = a.at.unwrap_or(f64::MAX);
        let kb = b.at.unwrap_or(f64::MAX);
        ka.partial_cmp(&kb)
            .unwrap_or(std::cmp::Ordering::Equal)
            // documented tie rule (guard 7): source index
            .then(a.index.cmp(&b.index))
    });
    // Sections with no `at` cannot be placed; drop them (same rule
    // as today's parser).
    doc.sections.retain(|s| s.at.is_some());
    for (k, s) in doc.sections.iter_mut().enumerate() {
        s.index = k; // renumber to placement order for identity
    }
    let first_at = doc.sections.first().and_then(|s| s.at).unwrap_or(0.0);
    let spb = doc.seconds_per_beat();
    for i in 0..doc.sections.len() {
        let at = doc.sections[i].at.unwrap_or(first_at);
        let start_s = at - first_at;
        let dur_s = if let Some(d) = doc.sections[i].dwell {
            d
        } else if let Some(next) = doc.sections.get(i + 1).and_then(|s| s.at) {
            next - at
        } else {
            // The old DEFAULT_SECTION_DURATION (30s) survives only as
            // this fallback for the tail section.
            30.0
        };
        let s = &mut doc.sections[i];
        s.start_beat = (start_s / spb).round().max(0.0) as i64;
        s.duration_beats = (dur_s / spb).round().max(1.0) as i64;
    }
}

// ── disk entry points ────────────────────────────────────────

/// The path the reader picks for a song folder: the app-owned
/// `sections.json` (C2) if present, else the legacy `.sections.json`
/// that songrun.py still writes. Checked in that order so a folder
/// mid-transition loads the app's file.
pub fn sections_path(song_dir: &Path) -> PathBuf {
    let new = song_dir.join(SECTIONS_FILE);
    if new.exists() {
        return new;
    }
    song_dir.join(LEGACY_SECTIONS_FILE)
}

/// Read a song folder's sections document. Missing/unreadable files
/// return an empty document (schema 0) -- the dropdown lists every
/// song folder, benched or not, and APPLY on an unbached one shows an
/// empty timeline rather than failing.
pub fn read_song(song_dir: &Path) -> (SongDoc, Vec<String>) {
    let path = sections_path(song_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (SongDoc::empty(path), Vec::new());
    };
    let (mut doc, warnings) = parse_sections_json(&text, &path);
    // A file that parsed to nothing but existed is corrupt (R9):
    // schema stays 0 and the caller must keep the last-good document
    // and demand an explicit RELOAD -- never silently blank the view.
    doc.dirty = false;
    (doc, warnings)
}

/// True when the document looks like a real read (has sections or a
/// song path), used to guard the post-save re-parse (guard 11).
pub fn doc_is_solid(doc: &SongDoc) -> bool {
    doc.schema >= 1 && (!doc.sections.is_empty() || !doc.song.is_empty())
}

// ── the writer (R12) ─────────────────────────────────────────
// Hand-rolled JSON so both serializers match the scanner's output
// (§4.3); serde stays out of the crate (WP-A guard c).

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn json_num(n: f64) -> String {
    // Round-trip stable shortest form; beats are integers so only
    // `cps` needs a fractional print.
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Serialize the document back to schema v2 text. Integer beats
/// only (§4.4); `at` is written as null (bench telemetry never
/// rides the app's write path, §5).
pub fn serialize_song_doc(doc: &SongDoc) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"schema\": 2,\n");
    out.push_str(&format!("  \"song\": \"{}\",\n", json_escape(&doc.song)));
    out.push_str(&format!(
        "  \"beats_per_cycle\": {},\n",
        doc.beats_per_cycle
    ));
    out.push_str(&format!("  \"cps\": {},\n", json_num(doc.cps)));
    out.push_str("  \"sections\": [\n");
    for (k, s) in doc.sections.iter().enumerate() {
        let orbits: Vec<String> = s
            .orbits
            .iter()
            .map(|o| format!("\"{}\"", json_escape(o)))
            .collect();
        out.push_str("    {\n");
        out.push_str(&format!("      \"index\": {},\n", s.index));
        out.push_str(&format!(
            "      \"label\": \"{}\",\n",
            json_escape(&s.label)
        ));
        out.push_str(&format!("      \"start_beat\": {},\n", s.start_beat));
        out.push_str(&format!(
            "      \"duration_beats\": {},\n",
            s.duration_beats
        ));
        out.push_str(&format!(
            "      \"line_range\": {{ \"start\": {}, \"end\": {} }},\n",
            s.line_start, s.line_end
        ));
        out.push_str(&format!("      \"orbits\": [{}],\n", orbits.join(", ")));
        out.push_str(&format!(
            "      \"source\": \"{}\",\n",
            json_escape(&s.source)
        ));
        out.push_str("      \"at\": null\n");
        out.push_str(if k + 1 == doc.sections.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    out.push_str("  ]\n}\n");
    out
}

/// Atomic save (R12): write tmp, fsync, keep one `.bak` of the
/// previous file, rename over the target. On any failure the caller
/// leaves `dirty` true -- the change lives in the UI until a real
/// write lands. The target is always the app-owned `sections.json`,
/// so the legacy dot-file never gets a second writer (guard 21).
pub fn save_document(doc: &SongDoc) -> std::io::Result<PathBuf> {
    let target = doc
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Legacy loads save forward to the new name, not back over the
    // dot-file songrun.py owns.
    let final_path = if target == LEGACY_SECTIONS_FILE || target != SECTIONS_FILE {
        doc.path.parent().map(|p| p.join(SECTIONS_FILE)).unwrap_or(doc.path.clone())
    } else {
        doc.path.clone()
    };
    let tmp = final_path.with_extension("json.tmp");
    let bak = final_path.with_extension("json.bak");

    let text = serialize_song_doc(doc);
    {
        use std::io::Write;
        let mut fh = std::fs::File::create(&tmp)?;
        fh.write_all(text.as_bytes())?;
        fh.flush()?;
        fh.sync_all()?;
    }
    // One level of undo / crash backup (R11/R12): copy the current
    // file (if any) over the .bak, then rename tmp into place.
    if final_path.exists() {
        let _ = std::fs::copy(&final_path, &bak);
    }
    std::fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Restore the `.bak` over a corrupt current file (R9 half-written
/// state recovery). Returns true when a backup landed.
pub fn restore_backup(song_dir: &Path) -> bool {
    let target = song_dir.join(SECTIONS_FILE);
    let bak = target.with_extension("json.bak");
    bak.exists() && std::fs::copy(&bak, &target).is_ok()
}

// ── Slint model bridge ───────────────────────────────────────
// Convert the document into the generated GroupData / TrackData /
// SectionData structs the SongTimeline binds to. Beats, integers all
// the way (R5/R14). `is-active` stays a display field (everything
// read from the file is active); selection highlight is done by the
// component's sel-* properties, not by mutating the model.

pub fn to_group_model(doc: &SongDoc) -> ModelRc<GroupData> {
    // Route the flat section list into groups/lanes, sorted per lane
    // by (start_beat, index) -- stable, integer keys, no float
    // comparison anywhere (guard 7).
    let mut sorted: Vec<&DocSection> = doc.sections.iter().collect();
    sorted.sort_by_key(|s| (s.start_beat, s.index));

    let groups: Vec<GroupData> = GROUPS
        .iter()
        .map(|(name, prefix)| {
            let mut lanes: Vec<(usize, Vec<SectionData>)> = Vec::new();
            for s in &sorted {
                for orbit in &s.orbits {
                    let Some(rest) = orbit.strip_prefix(*prefix) else {
                        continue;
                    };
                    let Ok(idx) = rest.parse::<usize>() else {
                        continue;
                    };
                    if idx == 0 || idx > MAX_ORBITS_PER_GROUP {
                        continue;
                    }
                    let data = SectionData {
                        name: SharedString::from(s.label.as_str()),
                        start_beat: s.start_beat as i32,
                        duration_beats: s.duration_beats as i32,
                        is_active: true,
                        section_id: s.index as i32,
                    };
                    match lanes.iter_mut().find(|(i, _)| *i == idx) {
                        Some(slot) => slot.1.push(data),
                        None => lanes.push((idx, vec![data])),
                    }
                }
            }
            lanes.sort_by_key(|(i, _)| *i);
            let tracks: Vec<TrackData> = lanes
                .into_iter()
                .map(|(i, secs)| TrackData {
                    name: SharedString::from(format!("{prefix}{i}")),
                    is_soloed: false,
                    is_muted: false,
                    sections: ModelRc::new(VecModel::from_slice(&secs)),
                })
                .collect();
            GroupData {
                name: SharedString::from(*name),
                accent: slint::Color::default(),
                tracks: ModelRc::new(VecModel::from_slice(&tracks)),
            }
        })
        .collect();
    ModelRc::new(VecModel::from_slice(&groups))
}

/// Song folder names under `runs/`, sorted. Every folder is a
/// dropdown candidate; folders without any sections file load empty.
pub fn list_song_folders(runs_dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── orbit routing (unchanged from the seconds era) ───────

    #[test]
    fn routes_orbit_statements() {
        let src = "do\n  hush\n  l6 $ layer [density 0.25] $ stack [...]\n  b1 $ slowcat [...]";
        let mut got = orbits_in_source(src);
        got.sort();
        assert_eq!(got, vec!["b1".to_string(), "l6".to_string()]);
    }

    #[test]
    fn ignores_dollar_in_expressions() {
        let src = "do\n  s \"bd $ ~\"";
        assert!(orbits_in_source(src).is_empty());
    }

    #[test]
    fn dedups_repeated_orbit_in_one_section() {
        let src = "do\n  l3 $ ...\n  l3 $ ...";
        assert_eq!(orbits_in_source(src), vec!["l3".to_string()]);
    }

    #[test]
    fn accepts_a_b_l_prefixes_only() {
        let src = "do\n  a2 $ x\n  b4 $ y\n  l1 $ z\n  c1 $ w";
        let mut got = orbits_in_source(src);
        got.sort();
        assert_eq!(got, vec!["a2", "b4", "l1"]);
    }

    #[test]
    fn tolerates_spaces_before_dollar() {
        let src = "do\n  l3   $ x";
        assert_eq!(orbits_in_source(src), vec!["l3".to_string()]);
    }

    // ── v2 reading ───────────────────────────────────────────

    fn v2_doc() -> &'static str {
        r#"{
            "schema": 2,
            "song": "/home/endo/songs/05-ecdysis-108bpm.tidal",
            "beats_per_cycle": 4,
            "cps": 0.45,
            "sections": [
                { "index": 0, "label": ":ECDYSIS", "start_beat": 0, "duration_beats": 16,
                  "line_range": {"start": 12, "end": 40}, "orbits": ["l6"],
                  "source": "do\n  l6 $ layer [density 0.25]", "at": null },
                { "index": 1, "label": "es ésta", "start_beat": 16, "duration_beats": 8,
                  "line_range": {"start": 41, "end": 52}, "orbits": ["l4"],
                  "source": "do -- es ésta\n  l4 $ every 4 (fast 2)", "at": null },
                { "index": 2, "label": "mixed", "start_beat": 24, "duration_beats": 32,
                  "line_range": {"start": 53, "end": 90}, "orbits": ["b1", "l3", "a1"],
                  "source": "do\n  b1 $ slowcat [...]\n  l3 $ layer [...]\n  a1 $ ...",
                  "at": null }
            ]
        }"#
    }

    fn parsed_v2() -> SongDoc {
        let (doc, _w) = parse_sections_json(v2_doc(), Path::new("/tmp/sections.json"));
        doc
    }

    #[test]
    fn v2_keeps_explicit_beats_untouched() {
        // §4.1: no read path recomputes start/duration once present.
        let doc = parsed_v2();
        assert_eq!(doc.schema, 2);
        assert_eq!(doc.cps, 0.45);
        assert_eq!(doc.beats_per_cycle, 4);
        assert_eq!(doc.sections[0].start_beat, 0);
        assert_eq!(doc.sections[1].start_beat, 16);
        assert_eq!(doc.sections[2].duration_beats, 32);
        // Not ordered by start in the file? order is preserved anyway.
        assert_eq!(doc.length_beats(), 56);
    }

    #[test]
    fn v2_groups_route_by_orbits() {
        let doc = parsed_v2();
        // Routing happens in to_group_model; the document carries the
        // orbits, and the static group layout is BEATS/LEADS/AMBIENTS.
        let lanes: Vec<String> = doc
            .sections
            .iter()
            .flat_map(|s| s.orbits.clone())
            .collect();
        assert!(lanes.contains(&"b1".to_string()));
        assert!(lanes.contains(&"l6".to_string()));
        assert!(lanes.contains(&"a1".to_string()));
        let model = to_group_model(&doc);
        assert_eq!(model.row_count(), 3);
        let names: Vec<String> = (0..model.row_count())
            .map(|i| model.row_data(i).unwrap().name.to_string())
            .collect();
        assert_eq!(names, vec!["BEATS", "LEADS", "AMBIENTS"]);
    }

    #[test]
    fn v2_unicode_label_survives() {
        let doc = parsed_v2();
        assert_eq!(doc.sections[1].label, "es ésta");
    }

    #[test]
    fn literal_utf8_label_survives() {
        let text = r#"{
            "schema": 2, "cps": 0.5, "beats_per_cycle": 4,
            "sections": [
                { "index": 0, "label": "literal ésta", "start_beat": 0,
                  "duration_beats": 4, "orbits": ["l1"], "source": "do" }
            ]
        }"#;
        let (doc, _w) = parse_sections_json(text, Path::new("/tmp/sections.json"));
        assert_eq!(doc.sections[0].label, "literal ésta");
    }

    #[test]
    fn duration_floor_is_one_beat_not_half_a_second() {
        // Guard 6: the data floor moved from 0.5 s to 1 beat (R10);
        // 30px stays a render-only concern in timeline.slint.
        let text = r#"{
            "schema": 2, "cps": 0.5, "beats_per_cycle": 4,
            "sections": [
                { "index": 0, "label": "a", "start_beat": 0, "duration_beats": 0 },
                { "index": 1, "label": "b", "start_beat": 2, "duration_beats": -3 }
            ]
        }"#;
        let (doc, warnings) = parse_sections_json(text, Path::new("/tmp/sections.json"));
        assert_eq!(doc.sections[0].duration_beats, 1);
        assert_eq!(doc.sections[1].duration_beats, 1);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn negative_start_clamped() {
        let text = r#"{
            "schema": 2, "cps": 0.5, "beats_per_cycle": 4,
            "sections": [ { "index": 0, "label": "a", "start_beat": -4, "duration_beats": 8 } ]
        }"#;
        let (doc, warnings) = parse_sections_json(text, Path::new("/tmp/sections.json"));
        assert_eq!(doc.sections[0].start_beat, 0);
        assert_eq!(warnings.len(), 1);
    }

    // ── round trip (the §4 invariant) ────────────────────────

    #[test]
    fn round_trip_is_identity() {
        // parse(save(document)) == document, with a manual edit in
        // the middle so edited values are exercised too.
        let mut doc = parsed_v2();
        doc.sections[1].start_beat = 18;
        doc.sections[1].duration_beats = 10;
        let text = serialize_song_doc(&doc);
        let (back, warnings) = parse_sections_json(&text, Path::new("/tmp/sections.json"));
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(back.sections, doc.sections);
        assert_eq!(back.cps, doc.cps);
        assert_eq!(back.beats_per_cycle, doc.beats_per_cycle);
        assert_eq!(back.song, doc.song);
    }

    #[test]
    fn round_trip_survives_tricky_strings() {
        let mut doc = parsed_v2();
        doc.sections[0].label = "quote \" back\\slash \n newline é中".into();
        doc.sections[0].source = "do\n  b1 $ s \"bd*2\"".into();
        let text = serialize_song_doc(&doc);
        let (back, _) = parse_sections_json(&text, Path::new("/tmp/sections.json"));
        assert_eq!(back.sections[0].label, doc.sections[0].label);
        assert_eq!(back.sections[0].source, doc.sections[0].source);
    }

    #[test]
    fn cps_prints_as_extracted_value() {
        // 180/60/4 = 0.75; 142/60/4 is a long decimal and must
        // round-trip exactly (both sides compare parsed floats).
        let mut doc = parsed_v2();
        doc.cps = 142.0 / 60.0 / 4.0;
        let text = serialize_song_doc(&doc);
        let (back, _) = parse_sections_json(&text, Path::new("/tmp/sections.json"));
        assert_eq!(back.cps, doc.cps);
    }

    // ── legacy fallback (R20-era bench files) ────────────────

    fn ecdysis_like() -> &'static str {
        r#"{
            "song": "/home/endo/songs/05-ecdysis-108bpm.tidal",
            "floor_synths": 90,
            "sections": [
                { "label": ":ECDYSIS", "dwell": null,
                  "source": "do\n  l6 $ layer [density 0.25] $ stack [...]",
                  "line": 2, "at": 1000.0 },
                { "label": "section-2", "dwell": null,
                  "source": "do\n  l5 $ layer [stut 4 0.5 1]",
                  "line": 28, "at": 1005.0 },
                { "label": "es ésta", "dwell": null,
                  "source": "do -- es ésta\n  l4 $ every 4 (fast 2) $ layer [...]",
                  "line": 40, "at": 1010.0 },
                { "label": "mixed", "dwell": 8.0,
                  "source": "do\n  b1 $ slowcat [...]\n  l3 $ layer [...]\n  a1 $ ...",
                  "line": 53, "at": 1015.0 }
            ]
        }"#
    }

    #[test]
    fn legacy_parses_with_unchanged_shape() {
        // The test list §11: today's file loads via the FALLBACK
        // path, same sections, same relative placement. At
        // 120bpm/4bpc the default puts one beat at 0.5 s, so the
        // 5s sampling cadence becomes 10-beat steps.
        let (doc, warnings) =
            parse_sections_json(ecdysis_like(), Path::new("/tmp/.sections.json"));
        assert_eq!(doc.schema, 1, "marked legacy");
        assert!(!warnings.is_empty(), "fallback path warns");
        assert_eq!(doc.sections.len(), 4);
        assert_eq!(doc.sections[0].label, ":ECDYSIS");
        // start offsets 0, 10, 20, 30 beats (5 s apart at 0.5 s/beat)
        let starts: Vec<i64> = doc.sections.iter().map(|s| s.start_beat).collect();
        assert_eq!(starts, vec![0, 10, 20, 30]);
        // dwell 8.0s -> 16 beats on the last, gap 5s -> 10 beats elsewhere
        assert_eq!(doc.sections[3].duration_beats, 16);
        assert_eq!(doc.sections[0].duration_beats, 10);
    }

    #[test]
    fn legacy_groups_and_orbits_route_from_source() {
        let (doc, _w) = parse_sections_json(ecdysis_like(), Path::new("/tmp/x"));
        // l6/l5/l4 leads, b1 beat, a1 ambient, l3 from "mixed".
        let mut orbits: Vec<String> = doc.sections.iter().flat_map(|s| s.orbits.clone()).collect();
        orbits.sort();
        orbits.dedup();
        assert_eq!(orbits, vec!["a1", "b1", "l3", "l4", "l5", "l6"]);
    }

    #[test]
    fn legacy_labeled_by_first_at_order() {
        // Sorting by `at` with equal stamps must keep source order
        // (guard 7). Reversed input order proves the sort ran.
        let text = r#"{
            "song": "/x.tidal",
            "sections": [
                { "label": "second", "source": "do\n  l2 $ x", "at": 1005.0 },
                { "label": "first",  "source": "do\n  l1 $ x", "at": 1000.0 },
                { "label": "tie-a",  "source": "do\n  l3 $ x", "at": 1005.0 },
                { "label": "tie-b",  "source": "do\n  l4 $ x", "at": 1005.0 }
            ]
        }"#;
        let (doc, _w) = parse_sections_json(text, Path::new("/tmp/x"));
        let labels: Vec<&str> = doc.sections.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["first", "second", "tie-a", "tie-b"]);
    }

    #[test]
    fn legacy_default_tail_duration() {
        // The last section has no next `at`: 30 s default -> 60 beats
        // at the fallback tempo (old DEFAULT_SECTION_DURATION lives
        // on only here).
        let text = r#"{
            "song": "/x.tidal",
            "sections": [ { "label": "only", "source": "do\n  l1 $ x", "at": 1000.0 } ]
        }"#;
        let (doc, _w) = parse_sections_json(text, Path::new("/tmp/x"));
        assert_eq!(doc.sections[0].duration_beats, 60);
    }

    #[test]
    fn section_without_at_is_dropped() {
        let text = r#"{
            "sections": [
                { "label": "placed", "source": "do\n  l1 $ x", "at": 1000.0 },
                { "label": "no-time", "source": "do\n  l2 $ y" }
            ]
        }"#;
        let (doc, _w) = parse_sections_json(text, Path::new("/tmp/x"));
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].label, "placed");
    }

    // ── robustness ───────────────────────────────────────────

    #[test]
    fn missing_file_is_empty() {
        let (doc, _w) = read_song(Path::new("/nonexistent-hub-song-folder"));
        assert!(doc.is_empty());
        assert_eq!(doc.schema, 0);
        assert_eq!(doc.length_beats(), 0);
    }

    #[test]
    fn empty_string_is_empty() {
        let (doc, _w) = parse_sections_json("", Path::new("/tmp/x"));
        assert!(doc.is_empty());
        assert_eq!(doc.schema, 0);
    }

    #[test]
    fn truncated_file_yields_what_closed() {
        let truncated = r#"{
            "schema": 2, "cps": 0.5, "beats_per_cycle": 4,
            "sections": [
                { "index": 0, "label": "ok", "start_beat": 0, "duration_beats": 4 },
                { "index": 1, "label": "cut"#;
        let (doc, _w) = parse_sections_json(truncated, Path::new("/tmp/x"));
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].label, "ok");
    }

    #[test]
    fn corrupt_file_is_flagged_not_solid() {
        // guard 11 / R9: a real file that yields nothing must read as
        // not-solid so the caller keeps the last-good document.
        let (doc, _w) = parse_sections_json("{ not json", Path::new("/tmp/x"));
        assert!(!doc_is_solid(&doc));
        let (good, _w) = parsed_v2_solid();
        assert!(doc_is_solid(&good));
    }

    fn parsed_v2_solid() -> (SongDoc, ()) {
        (parsed_v2(), ())
    }

    #[test]
    fn default_label_for_unnamed_section() {
        let text = r#"{
            "schema": 2, "cps": 0.5, "beats_per_cycle": 4,
            "sections": [ { "index": 3, "start_beat": 0, "duration_beats": 4 } ]
        }"#;
        let (doc, _w) = parse_sections_json(text, Path::new("/tmp/x"));
        assert_eq!(doc.sections[0].label, "section-4");
    }

    // ── editing rules (R10/C4/R16) ───────────────────────────

    fn editable() -> SongDoc {
        parsed_v2()
    }

    #[test]
    fn step_start_shifts_without_touching_length() {
        // R16: shift, not stretch.
        let mut doc = editable();
        let before_len = doc.sections[2].duration_beats;
        assert!(doc.step_start(2, -1));
        assert_eq!(doc.sections[2].start_beat, 23);
        assert_eq!(doc.sections[2].duration_beats, before_len);
        assert!(doc.dirty, "a real move sets dirty");
    }

    #[test]
    fn step_start_rejects_negative() {
        let mut doc = editable();
        assert!(!doc.step_start(0, -1), "already at 0: clamped, no move, no dirty");
        assert_eq!(doc.sections[0].start_beat, 0);
        assert!(!doc.dirty, "a no-op move does not set dirty");
    }

    #[test]
    fn step_start_cannot_pass_the_next_start() {
        let mut doc = editable();
        // section 0 at 0, section 1 at 16: shifting forward 32 clamps
        // to 16 (equal is allowed, R10).
        assert!(doc.step_start(0, 32));
        assert_eq!(doc.sections[0].start_beat, 16);
        assert!(!doc.step_start(0, 5), "at the wall now: no movement");
    }

    #[test]
    fn step_start_cannot_precede_the_previous_start() {
        let mut doc = editable();
        assert!(doc.step_start(1, -20));
        assert_eq!(doc.sections[1].start_beat, 0, "equal to previous is the floor");
    }

    #[test]
    fn step_length_floors_at_one_beat() {
        let mut doc = editable();
        doc.sections[0].duration_beats = 1;
        assert!(!doc.step_length(0, -1), "1 beat is the floor (R10)");
        assert!(doc.step_length(0, 7));
        assert_eq!(doc.sections[0].duration_beats, 8);
    }

    #[test]
    fn lengthening_past_next_start_is_legal_overlap() {
        // C4: overlap is legal everywhere; only order is enforced.
        let mut doc = editable();
        assert!(doc.step_length(0, 100));
        assert!(doc.sections[0].duration_beats > 100);
        assert_eq!(doc.sections[1].start_beat, 16, "neighbour untouched");
    }

    #[test]
    fn stepping_unknown_index_is_inert() {
        let mut doc = editable();
        assert!(!doc.step_start(99, 1));
        assert!(!doc.step_length(99, 1));
        assert!(!doc.dirty);
    }

    // ── path choice across the C2 rename ─────────────────────

    #[test]
    fn new_name_wins_over_legacy_dotfile() {
        let dir = std::env::temp_dir().join("hub-sections-priority-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(SECTIONS_FILE), r#"{"schema":2,"cps":0.5,"beats_per_cycle":4,"song":"v2","sections":[]}"#).unwrap();
        std::fs::write(dir.join(LEGACY_SECTIONS_FILE), r#"{"song":"legacy","sections":[]}"#).unwrap();
        assert_eq!(sections_path(&dir), dir.join(SECTIONS_FILE));
        let (doc, _w) = read_song(&dir);
        assert_eq!(doc.song, "v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_dotfile_accepted_alone() {
        let dir = std::env::temp_dir().join("hub-sections-legacy-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join(LEGACY_SECTIONS_FILE),
            r#"{"song":"legacy","sections":[{"label":"a","source":"do\n  b1 $ x","at":10.0}]}"#,
        )
        .unwrap();
        let (doc, _w) = read_song(&dir);
        assert_eq!(doc.song, "legacy");
        assert_eq!(doc.schema, 1);
        // A legacy read saves forward to the new name only (guard 21).
        let saved = save_document(&doc).unwrap();
        assert_eq!(saved, dir.join(SECTIONS_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_and_bakes_once() {
        let dir = std::env::temp_dir().join("hub-sections-save-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut doc = editable();
        doc.path = dir.join(SECTIONS_FILE);
        let saved = save_document(&doc).unwrap();
        assert!(saved.exists());
        assert!(!dir.join("sections.json.tmp").exists(), "tmp consumed by rename");
        // first save: nothing to back up (file did not exist).
        doc.sections[0].label = "renamed".into();
        save_document(&doc).unwrap();
        assert!(dir.join("sections.json.bak").exists(), "one .bak (R11/R12)");
        let (back, _w) = read_song(&dir);
        assert_eq!(back.sections[0].label, "renamed");
        assert_eq!(back.sections, doc.sections);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_reparse_recovers_from_bak() {
        // R12: a kill during save leaves either the old file or the
        // .bak readable. Simulate the .bak restore leg.
        let dir = std::env::temp_dir().join("hub-sections-restore-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut doc = editable();
        doc.path = dir.join(SECTIONS_FILE);
        save_document(&doc).unwrap();
        doc.sections[0].label = "second".into();
        save_document(&doc).unwrap(); // .bak now holds the first save
        std::fs::write(dir.join(SECTIONS_FILE), "{ truncated").unwrap();
        assert!(restore_backup(&dir));
        // .bak holds the previous good save -- the first one, since
        // the second save is what the corrupt write destroyed.
        let (back, _w) = read_song(&dir);
        assert_eq!(back.sections[0].label, ":ECDYSIS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── disk listing ─────────────────────────────────────────

    #[test]
    fn list_song_folders_missing_dir_is_empty() {
        assert!(list_song_folders(Path::new("/nonexistent-runs-dir")).is_empty());
    }

    // ── real on-disk data ────────────────────────────────────
    // Guards against drift on the real runs/ folders. Skipped (no
    // panic) when the folder is absent; reads only the sections
    // files, never song sources.

    // Cross-check the Python scanner's writer against this reader
    // (§4.3: both serializers must agree). The artifact is emitted
    // by tools/test_tidalscan.py::test_emit_crosscheck_artifact;
    // skipped here when it has not been generated yet.
    #[test]
    fn python_scanned_fixture_round_trips_through_rust() {
        let f = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools/fixtures/crosscheck/sections.json");
        let Ok(text) = std::fs::read_to_string(&f) else {
            return; // python suite not run here; nothing to assert
        };
        let (doc, warnings) = parse_sections_json(&text, &f);
        assert_eq!(doc.schema, 2, "scanner writes schema 2");
        assert!(warnings.is_empty(), "clean v2 read: {warnings:?}");
        // Re-serialize with the Rust writer and re-read: the
        // invariant holds over the Python-produced document too.
        let back = serialize_song_doc(&doc);
        let (doc2, _) = parse_sections_json(&back, &f);
        assert_eq!(doc2.sections, doc.sections);
        assert_eq!(doc2.cps, doc.cps);
        assert_eq!(doc2.beats_per_cycle, doc.beats_per_cycle);
        // Values the Python suite asserts on the same fixture.
        assert_eq!(doc.sections.len(), 4);
        assert_eq!(doc.cps, 0.5); // setcps(120/60/4) -> 120/60/4
        assert_eq!(doc.beats_per_cycle, 4);
        assert_eq!(doc.sections[0].duration_beats, 8);
        assert_eq!(doc.sections[1].start_beat, 8);
        assert_eq!(doc.sections[2].start_beat, 12);
        assert_eq!(doc.sections[3].start_beat, 28);
    }

    #[test]
    fn real_ecdysis_parses_into_all_three_groups() {
        let dir = std::path::Path::new(super::RUNS_DIR).join("ecdysis");
        let (doc, _w) = read_song(&dir);
        if doc.is_empty() {
            return; // not scanned here; nothing to assert
        }
        assert!(doc.length_beats() > 0);
        let orbits: Vec<&str> = doc
            .sections
            .iter()
            .flat_map(|s| s.orbits.iter().map(|o| o.as_str()))
            .collect();
        assert!(
            orbits.iter().any(|o| o.starts_with('b'))
                && orbits.iter().any(|o| o.starts_with('l'))
                && orbits.iter().any(|o| o.starts_with('a')),
            "ecdysis uses beats, leads and ambients"
        );
    }
}

// Small debug helper used by tests only: the static group layout.
