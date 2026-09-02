// ─────────────────────────────────────────────────────────────
// Rust-side label strings. Status text and colors that the UI
// derives from booleans live entirely in the Slint files; this
// module holds only the strings Rust itself still needs (label
// sanitization fallbacks, mute-group display names).
// ─────────────────────────────────────────────────────────────

// ── recording display ──────────────────────────────────────
pub const REC_STARTING_ELAPSED: &str = "00:00:00";
pub const REC_FALLBACK_SESSION: &str = "session";

// ── mute groups ────────────────────────────────────────────
pub const MUTE_GROUP_NAMES: [&str; 3] = ["BEATS", "LEADS", "AMBIENTS"];