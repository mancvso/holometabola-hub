// ─────────────────────────────────────────────────────────────
// WP-D: the beat clock and the in-app player (R7/R8).
//
// Musical time lives here: an integer-beat counter rebased by
// `resetCycles` / re-rated by `setcps`, never a free-running
// wall-clock translation of section starts. The app is the single
// owner of ghci's stdin, so the fire loop sends the wrapped blocks
// itself (R8: no external player, no /proc writer, no arbitration).
//
// Interception (R7): every line that reaches ghci goes through
// `send_line` in main.rs -- the panel buttons, the editor relay
// (serve_editor forwards each relayed line into send_line), and the
// player's own fire text (R21). One interceptor at that choke point
// covers all three call sites: `note_sent` re-bases the counter on
// `resetCycles` and updates the rate on `setcps(bpm/60/bpc)`.
//
// Rate convention (C8/R19, frozen): the document stores cps exactly
// as extracted -- bpm/60/bpc -- so beats-per-second = cps * bpc and
// one beat lasts 60/bpm seconds. A section of `duration_beats`
// dwells `duration_beats / (cps * bpc)` seconds (R19/WP-D).
// ─────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Latency compensation (C5): a single constant in one place,
/// applied to the trigger time only. Never drawn on the playhead,
/// never exposed in the UI. Do not over-engineer.
pub const TRIGGER_OFFSET_S: f64 = 0.0;

/// The shared beat clock. `resetCycles` sets beat zero to *now* and
/// bumps `generation`; the player re-arms its triggers on a
/// generation change (mid-song rebase = sections fire again as the
/// beat passes their starts).
pub struct Clock {
    /// Instant at which the current beat count started.
    base: Mutex<Instant>,
    /// cps * 1e9 (fixed-point; f64 atomics do not exist). Song-wide
    /// rate from the first `setcps` (R19); updated only by
    /// interception, never derived.
    cps_bits: AtomicU64,
    beats_per_cycle: AtomicI64,
    generation: AtomicU64,
}

impl Clock {
    fn new() -> Self {
        Clock {
            base: Mutex::new(Instant::now()),
            cps_bits: AtomicU64::new(0),
            beats_per_cycle: AtomicI64::new(4),
            generation: AtomicU64::new(0),
        }
    }

    pub fn cps(&self) -> f64 {
        self.cps_bits.load(Ordering::SeqCst) as f64 / 1e9
    }

    pub fn beats_per_cycle(&self) -> i64 {
        self.beats_per_cycle.load(Ordering::SeqCst)
    }

    /// Beats per second under the C8 extraction rule.
    pub fn beats_per_second(&self) -> f64 {
        self.cps() * self.beats_per_cycle() as f64
    }

    pub fn set_rate(&self, cps: f64, bpc: i64) {
        // Keep the beat continuous across a rate change: freeze the
        // current beat, swap the rate, re-anchor the base so the same
        // beat still reads the same right now.
        let before = self.beat();
        let bps_new = cps * bpc as f64;
        if bps_new > 0.0 {
            let back = std::time::Duration::from_secs_f64((before / bps_new).max(0.0));
            *self.base.lock().unwrap() = Instant::now() - back;
        } else {
            *self.base.lock().unwrap() = Instant::now();
        }
        self.cps_bits
            .store((cps * 1e9).round().max(0.0) as u64, Ordering::SeqCst);
        self.beats_per_cycle.store(bpc, Ordering::SeqCst);
    }

    /// Current beat, fractional. Wall-clock inside the clock only --
    /// the document and every trigger decision speak beats.
    pub fn beat(&self) -> f64 {
        let base = *self.base.lock().unwrap();
        base.elapsed().as_secs_f64() * self.beats_per_second()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Beat zero is *now* (a `resetCycles` went through the pipe).
    pub fn rebase(&self) {
        *self.base.lock().unwrap() = Instant::now();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Inspect one outbound line (R7 interceptor body): re-base on
    /// `resetCycles`, re-rate on the first `setcps` parameter triple
    /// found. Returns a note for the log when something was seen.
    ///
    /// C8/guard 18: text extraction of delimited numbers only --
    /// the expression is never evaluated and identifiers are refused.
    pub fn note_sent(&self, text: &str) -> Option<String> {
        let mut note = None;
        if let Some((bpm, _sixty, bpc)) = parse_setcps(text) {
            let cps = bpm / 60.0 / bpc as f64;
            self.set_rate(cps, bpc);
            note = Some(format!("setcps: rate -> bpm={bpm} bpc={bpc} (beat kept continuous)"));
        }
        // `resetCycles` may ride in the same wrapped block; order
        // matters: setcps first (rate), then the rebase (R21 -- beat
        // zero must land at the moment the block reaches ghci).
        if contains_reset_cycles(text) {
            self.rebase();
            let suffix = "resetCycles: beat 0 re-based";
            note = Some(match note {
                Some(n) => format!("{n}; {suffix}"),
                None => suffix.to_string(),
            });
        }
        note
    }
}

pub fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(Clock::new)
}

// ── text extraction (never eval, guard 18) ───────────────────

/// `resetCycles` as a word. Comments inside a wrapped block still
/// trip it -- matching the plan's interception rule (the app reads
/// what the human sends; a commented-out statement passing through
/// ghci's stdin in a `:{ :}` block is rare and benign).
fn contains_reset_cycles(text: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == "resetCycles")
}

/// Extract `(bpm, 60, bpc)` from `setcps(180/60/4)` or the spaced
/// form `setcps (142/60/4)`. Returns None for any other shape (an
/// identifier, a single literal, a malformed split) -- the caller
/// then leaves the rate untouched rather than guessing.
fn parse_setcps(text: &str) -> Option<(f64, f64, i64)> {
    let idx = text.find("setcps")?;
    let rest = text[idx + "setcps".len()..].trim_start();
    let inner = rest.strip_prefix('(')?;
    let close = inner.find(')')?;
    let params: Vec<&str> = inner[..close].split('/').map(str::trim).collect();
    if params.len() != 3 {
        return None;
    }
    let bpm: f64 = params[0].parse().ok()?;
    let sixty: f64 = params[1].parse().ok()?;
    let bpc: i64 = params[2].parse().ok()?;
    if sixty != 60.0 || bpc < 1 || bpm <= 0.0 {
        return None;
    }
    Some((bpm, sixty, bpc))
}

// ── the fire loop ────────────────────────────────────────────
// The loop itself lives beside the ghci pipe (main.rs): it runs
// while its minted session id is current, polls the clock at a
// 5 ms cadence, and fires due sections through `send_line` -- the
// same choke point the interceptor watches, which is what makes
// the player's own fire text a re-base source (R21).

/// Session counter: each PLAY mints a new id; the fire loop runs
/// while its id is the current one. STOP sets it to 0.
pub fn session_counter() -> &'static AtomicU64 {
    static SESSION: AtomicU64 = AtomicU64::new(0);
    &SESSION
}

/// Wrap a section body for ghci: the `:{ ... :}` form is mandatory
/// (WP-B: ghci interprets one line at a time, an unwrapped multi-
/// line `do` silently runs only its first line). The label rides
/// along as a comment so the log shows what fired.
pub fn wrap(label: &str, source: &str) -> String {
    format!(":{{\n-- {label}\n{source}\n:}}")
}

/// True while this session is the live one.
pub fn session_active(id: u64) -> bool {
    id != 0 && session_counter().load(Ordering::SeqCst) == id
}

/// Fire decision (pure, testable without a ghci): given the clock
/// beat and a section's placement, must it fire now? The trigger
/// offset (C5) moves the *trigger time* only, by TRIGGER_OFFSET_S
/// seconds converted to beats on the current rate.
pub fn should_fire(beat: f64, start_beat: i64, fired: bool) -> bool {
    if fired {
        return false;
    }
    let offset_beats = TRIGGER_OFFSET_S * clock().beats_per_second();
    beat + offset_beats >= start_beat as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_setcps_plain_and_spaced() {
        assert_eq!(parse_setcps("setcps(142/60/4)"), Some((142.0, 60.0, 4)));
        assert_eq!(parse_setcps("setcps (142/60/4)"), Some((142.0, 60.0, 4)));
        assert_eq!(parse_setcps("  x $ setcps( 90 / 60 / 4 )"), Some((90.0, 60.0, 4)));
        // 8 beats per cycle is a scratch form in the corpus, but the
        // extractor is mechanical (C8): three delimited numbers.
        assert_eq!(parse_setcps("setcps(160/60/8)"), Some((160.0, 60.0, 8)));
    }

    #[test]
    fn parse_setcps_refuses_anything_else() {
        // Guard 18: never eval, never accept identifiers.
        assert_eq!(parse_setcps("setcps myCps"), None);
        assert_eq!(parse_setcps("setcps(180/61/4)"), None); // middle is 60 or nothing
        assert_eq!(parse_setcps("setcps(0.5)"), None);
        assert_eq!(parse_setcps("setcps(a/60/4)"), None);
        assert_eq!(parse_setcps("setcps(180/60/4"), None);
        assert_eq!(parse_setcps("resetCycles"), None);
    }

    #[test]
    fn reset_cycles_word_boundary() {
        assert!(contains_reset_cycles("do\n  resetCycles\n  b1 $ x"));
        assert!(contains_reset_cycles("resetCycles"));
        assert!(!contains_reset_cycles("resetCyclesFoo"));
        assert!(!contains_reset_cycles("nope"));
    }

    #[test]
    fn note_sent_rebases_and_rerates() {
        let c = Clock::new();
        c.set_rate(180.0 / 60.0 / 4.0, 4);
        assert!((c.beats_per_second() - 3.0).abs() < 1e-6);

        // The rate change keeps the beat continuous (no re-zero).
        let b0 = c.beat();
        let note = c.note_sent("setcps(120/60/4)").unwrap();
        assert!(note.contains("rate"));
        let b1 = c.beat();
        assert!((b1 - b0).abs() < 0.1, "beat preserved across setcps: {b0} -> {b1}");

        // resetCycles sets beat 0 and bumps the generation.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let g = c.generation();
        let note = c
            .note_sent(":{\n-- s1\n  resetCycles\n:}")
            .unwrap();
        assert!(note.contains("beat 0"));
        assert_eq!(c.generation(), g + 1);
        assert!(c.beat() < 0.2);
        assert!(c.note_sent("b1 $ s \"bd\"").is_none());
    }

    #[test]
    fn dwell_matches_beat_math() {
        // R19/WP-D: dwell = duration_beats / (cps * bpc) seconds,
        // i.e. duration_beats * 60/bpm. 16 beats at 120bpm = 8s.
        let c = Clock::new();
        c.set_rate(120.0 / 60.0 / 4.0, 4);
        let dwell = 16.0 / (c.cps() * c.beats_per_cycle() as f64);
        assert!((dwell - 8.0).abs() < 1e-9);
    }

    #[test]
    fn should_fire_order_and_fired_latch() {
        let c = Clock::new();
        c.set_rate(0.5, 4); // 2 beats/s
        assert!(!should_fire(0.0, 1, false), "not yet");
        assert!(should_fire(1.0, 1, false), "exactly at start: fire");
        assert!(should_fire(9.0, 1, false), "past start: fire");
        assert!(!should_fire(9.0, 1, true), "already fired: latch");
    }

    #[test]
    fn wrap_keeps_the_ghci_block_form() {
        // WP-B guard: dropping the wrapper breaks playback invisibly.
        let w = wrap("intro", "do\n  b1 $ s \"bd\"");
        assert!(w.starts_with(":{\n-- intro\ndo\n"));
        assert!(w.ends_with("\n:}"));
    }

    #[test]
    fn session_counter_gates_the_loop() {
        session_counter().store(7, Ordering::SeqCst);
        assert!(session_active(7));
        assert!(!session_active(8));
        session_counter().store(0, Ordering::SeqCst);
        assert!(!session_active(7), "stop deactivates every session");
    }
}
