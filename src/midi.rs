// MIDI device enumeration via aconnect. UI-only for now: the Reconnect
// button logs a stub; the real reconnect will send the device setup code
// to sclang (the same shape as the Midi Through block in startup.scd).
use std::collections::BTreeMap;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct MidiDevice {
    pub id: String,      // "14:0"
    pub name: String,    // "Midi Through Midi Through Port-0"
    pub direction: String, // "in" / "out" / "in+out"
}

// `aconnect -i -l` (and -o) prints:
//
//   client 14: 'Midi Through' [type=kernel]
//       0  'Midi Through Port-0'
//       Connecting to: 128:0
//
// Client 0 is the kernel's Timer/Announce pair, not a device. The output is
// localized, so LC_ALL=C pins the English shape; the port field is still
// validated as numeric so "Connecting to:" lines can never parse as ports.
fn collect(flag: &str, dir: &str, into: &mut BTreeMap<String, MidiDevice>) {
    let Ok(out) = Command::new("aconnect")
        .args([flag, "-l"])
        .env("LC_ALL", "C")
        .output()
    else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);

    let unquote = |s: &str| {
        s.trim()
            .trim_matches(|c| matches!(c, '\'' | '\u{00ab}' | '\u{00bb}'))
            .trim()
            .to_string()
    };

    let mut client: Option<(String, String)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("client ") {
            let (num, rest) = rest.split_once(':').unwrap_or((rest, ""));
            let num = num.trim();
            client = if num == "0" {
                None
            } else {
                let name = rest.split('[').next().unwrap_or_default();
                Some((num.to_string(), unquote(name)))
            };
        } else if let Some((cnum, cname)) = &client {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (port, pname) = trimmed
                .split_once(char::is_whitespace)
                .unwrap_or((trimmed, ""));
            let pname = unquote(pname);
            if pname.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let id = format!("{cnum}:{port}");
            match into.get_mut(&id) {
                Some(d) => {
                    if !d.direction.contains(dir) {
                        d.direction.push('+');
                        d.direction.push_str(dir);
                    }
                }
                None => {
                    into.insert(
                        id.clone(),
                        MidiDevice {
                            id,
                            name: format!("{cname} {pname}"),
                            direction: dir.to_string(),
                        },
                    );
                }
            }
        }
    }
}

pub fn list_devices() -> Vec<MidiDevice> {
    let mut map = BTreeMap::new();
    collect("-i", "in", &mut map);
    collect("-o", "out", &mut map);
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Kernel client 14 (Midi Through) is always present on Linux, so this
    // exercises the real, localized aconnect output end to end.
    #[test]
    fn parses_real_aconnect_output() {
        let devs = list_devices();
        assert!(
            devs.iter().any(|d| d.id == "14:0"),
            "Midi Through port missing from {devs:?}"
        );
    }
}
