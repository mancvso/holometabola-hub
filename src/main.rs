use slint::{Color, ComponentHandle};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

slint::include_modules!();

static LATES_COUNT: AtomicU32 = AtomicU32::new(0);

fn log_line(ui_weak: &slint::Weak<AppWindow>, line: String) {
    println!("{line}");

    let is_late = line
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("late"));

    let lates = if is_late {
        LATES_COUNT.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        LATES_COUNT.load(Ordering::Relaxed)
    };

    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_lates_count(lates as i32);
        }
    });
}

// Asynchronously inspect active ports via native Linux commands
fn is_process_running(port: &str) -> bool {
    let output = Command::new("pgrep")
        .args(["-f", &format!("scsynth -u {}", port)])
        .output();

    if let Ok(out) = output {
        !out.stdout.is_empty()
    } else {
        false
    }
}

fn kill_server(ui_weak: &slint::Weak<AppWindow>, port: &str) {
    let pattern = format!("scsynth -u {}", port);
    log_line(ui_weak, format!("$ pkill -f \"{pattern}\""));
    let _ = Command::new("pkill").args(["-f", &pattern]).status();
}

fn kill_orbits(ui_weak: &slint::Weak<AppWindow>) {
    kill_server(ui_weak, "57110");
    let pattern = format!("sclang -D {}", ORBITS_BOOT_FILE);
    log_line(ui_weak, format!("$ pkill -f \"{pattern}\""));
    let _ = Command::new("pkill").args(["-f", &pattern]).status();
}

async fn boot_scsynth(
    name: &'static str,
    port: &'static str,
    args: Vec<&'static str>,
    ui_weak: slint::Weak<AppWindow>,
) {
    if is_process_running(port) {
        log_line(
            &ui_weak,
            format!("[{name}] already running on {port}, skipping boot"),
        );
        return;
    }

    log_line(&ui_weak, format!("[{name}] booting on port {port}..."));
    log_line(&ui_weak, format!("[{name}] $ taskset {}", args.join(" ")));

    let mut child = match tokio::process::Command::new("taskset")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log_line(&ui_weak, format!("[{name}] failed to boot: {e}"));
            return;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log_line(&ui_weak, format!("[{name}] {line}"));
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log_line(&ui_weak, format!("[{name}] {line}"));
            }
        });
    }

    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

const ORBITS_BOOT_FILE: &str = "/home/endo/optimize/startup.scd";

const ONYX_CLIENT: &str = "Onyx Artist 1-2 Pro";

fn orbits_connect_pairs() -> Vec<(String, String)> {
    (1..=36)
        .map(|i| {
            let src = format!("SuperCollider:out_{i}");
            let dst = if i % 2 == 1 {
                format!("{ONYX_CLIENT}:playback_AUX0")
            } else {
                format!("{ONYX_CLIENT}:playback_AUX1")
            };
            (src, dst)
        })
        .collect()
}

fn connect_orbits(ui_weak: slint::Weak<AppWindow>) {
    for (src, dst) in orbits_connect_pairs() {
        log_line(&ui_weak, format!("$ jack_connect {src} \"{dst}\""));
        match Command::new("jack_connect").arg(&src).arg(&dst).status() {
            Ok(status) if status.success() => {
                log_line(&ui_weak, format!("[orbits] connected {src} -> {dst}"));
            }
            Ok(status) => {
                log_line(
                    &ui_weak,
                    format!("[orbits] jack_connect {src} -> {dst} exited with {status}"),
                );
            }
            Err(e) => {
                log_line(
                    &ui_weak,
                    format!("[orbits] jack_connect {src} -> {dst} failed: {e}"),
                );
            }
        }
    }
    log_line(&ui_weak, "[orbits] connect complete".into());
}

fn orbits_scsynth_args() -> Vec<&'static str> {
    vec![
        "-c", "4", "chrt", "-f", "80", "pw-jack", "-p", "128", "scsynth", "-u", "57110", "-H",
        "orbits", "-S", "48000", "-z", "128", "-i", "0", "-o", "36", "-a", "256", "-b", "8192",
        "-m", "262144", "-L",
    ]
}

fn orbits_sclang_args() -> Vec<&'static str> {
    vec![
        "-c",
        "4",
        "chrt",
        "-f",
        "80",
        "pw-jack",
        "-p",
        "128",
        "sclang",
        ORBITS_BOOT_FILE,
    ]
}

fn vocals_args() -> Vec<&'static str> {
    vec![
        "-c", "5", "chrt", "-f", "80", "pw-jack", "-p", "64", "scsynth", "-u", "57111", "-H",
        "vocals", "-S", "48000", "-z", "64", "-i", "2", "-o", "2", "-a", "64", "-m", "65536", "-L",
    ]
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    // Spawn async telemetry engine checking scsynth status
    {
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            loop {
                let orbits_running = is_process_running("57110");
                let vocals_running = is_process_running("57111");

                let ui_weak = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        if orbits_running {
                            ui.set_orbits_status("ONLINE".into());
                            ui.set_orbits_color(Color::from_rgb_u8(85, 255, 85));
                        } else {
                            ui.set_orbits_status("CRASHED / OFF".into());
                            ui.set_orbits_color(Color::from_rgb_u8(255, 85, 85));
                        }

                        if vocals_running {
                            ui.set_vocals_status("ONLINE".into());
                            ui.set_vocals_color(Color::from_rgb_u8(85, 255, 85));
                        } else {
                            ui.set_vocals_status("CRASHED / OFF".into());
                            ui.set_vocals_color(Color::from_rgb_u8(255, 85, 85));
                        }
                    }
                });

                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    // Handle UI interaction callbacks safely
    {
        let ui_weak = ui_weak.clone();
        ui.on_boot_orbits_scsynth(move || {
            tokio::spawn(boot_scsynth(
                "orbits",
                "57110",
                orbits_scsynth_args(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_boot_orbits_sclang(move || {
            tokio::spawn(boot_scsynth(
                "orbits",
                "57110",
                orbits_sclang_args(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_connect_orbits(move || {
            let ui_weak = ui_weak.clone();
            tokio::task::spawn_blocking(move || connect_orbits(ui_weak));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_boot_vocals(move || {
            tokio::spawn(boot_scsynth(
                "vocals",
                "57111",
                vocals_args(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_orbits(move || {
            log_line(
                &ui_weak,
                "[orbits] panic triggered: ending port 57110".into(),
            );
            kill_orbits(&ui_weak);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_vocals(move || {
            log_line(
                &ui_weak,
                "[vocals] panic triggered: ending port 57111".into(),
            );
            kill_server(&ui_weak, "57111");
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_force_restart_all(move || {
            log_line(
                &ui_weak,
                "[system] force restart: clearing all scsynth/sclang instances".into(),
            );
            log_line(&ui_weak, "$ pkill -f scsynth".into());
            let _ = Command::new("pkill").arg("-f").arg("scsynth").status();
            log_line(&ui_weak, "$ pkill -f sclang".into());
            let _ = Command::new("pkill").arg("-f").arg("sclang").status();
        });
    }

    ui.run()
}
