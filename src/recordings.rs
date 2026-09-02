// Browsing past recording sessions. Layout comes from recording.scd:
// recordings/<session>_<timestamp>/<stem>.wav, one stem per orbit.
// Durations are derived from file size -- stems are int24 stereo 48 kHz,
// so no probing tool is needed.
use std::path::{Path, PathBuf};

pub const RECORDINGS_DIR: &str = "/home/endo/Studio/Hub/recordings";

const BYTES_PER_SECOND: u64 = 48000 * 2 * 3;

#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub path: PathBuf,
    pub stems: usize,
}

#[derive(Debug, Clone)]
pub struct Stem {
    pub name: String,
    pub size: String,
    pub duration: String,
}

fn count_wavs(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "wav"))
                .count()
        })
        .unwrap_or(0)
}

// Newest first: the timestamp suffix makes lexicographic order
// chronological.
pub fn list_sessions() -> Vec<Session> {
    let Ok(rd) = std::fs::read_dir(RECORDINGS_DIR) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let path = e.path();
            Session {
                name: e.file_name().to_string_lossy().into_owned(),
                stems: count_wavs(&path),
                path,
            }
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn fmt_duration(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The recordings dir may not exist yet (it is created by recording.scd on
    // the first REC START); scanning must degrade to an empty list.
    #[test]
    fn missing_dir_is_empty() {
        assert!(list_stems(Path::new("/nonexistent-hub-session")).is_empty());
    }

    #[test]
    fn duration_math() {
        assert_eq!(fmt_duration(60), "00:01:00".to_string());
        assert_eq!(fmt_duration(3661), "01:01:01".to_string());
    }
}

pub fn list_stems(dir: &Path) -> Vec<Stem> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Stem> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "wav"))
        .filter_map(|e| {
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            // WAV header is ~78 bytes; negligible against stem sizes.
            let secs = bytes.saturating_sub(78) / BYTES_PER_SECOND;
            Some(Stem {
                name: e.file_name().to_string_lossy().into_owned(),
                size: format!("{:.1} MB", bytes as f64 / 1_000_000.0),
                duration: fmt_duration(secs),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
