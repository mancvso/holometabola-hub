use slint::{Color, ComponentHandle};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

slint::include_modules!();

const MAX_LOG_LINES: usize = 200;

type LogBuf = Arc<Mutex<Vec<String>>>;

fn push_log(log: &LogBuf, ui_weak: &slint::Weak<AppWindow>, line: String) {
    let joined = {
        let mut lines = log.lock().unwrap();
        lines.push(line);
        if lines.len() > MAX_LOG_LINES {
            let excess = lines.len() - MAX_LOG_LINES;
            lines.drain(0..excess);
        }
        lines.join("\n")
    };

    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_log_output(joined.into());
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

fn kill_server(port: &str) {
    let _ = Command::new("pkill")
        .args(["-f", &format!("scsynth -u {}", port)])
        .status();
}

async fn boot_scsynth(
    name: &'static str,
    port: &'static str,
    args: Vec<&'static str>,
    log: LogBuf,
    ui_weak: slint::Weak<AppWindow>,
) {
    if is_process_running(port) {
        push_log(
            &log,
            &ui_weak,
            format!("[{name}] already running on {port}, skipping boot"),
        );
        return;
    }

    push_log(
        &log,
        &ui_weak,
        format!("[{name}] booting on port {port}..."),
    );

    let mut child = match tokio::process::Command::new("taskset")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            push_log(&log, &ui_weak, format!("[{name}] failed to boot: {e}"));
            return;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_log(&log, &ui_weak, format!("[{name}] {line}"));
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_log(&log, &ui_weak, format!("[{name}] {line}"));
            }
        });
    }

    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

fn orbits_args() -> Vec<&'static str> {
    vec![
        "-c", "4", "chrt", "-f", "80", "pw-jack", "scsynth", "-u", "57110", "-H", "orbits", "-S",
        "48000", "-z", "64", "-i", "0", "-o", "36", "-a", "256", "-b", "8192", "-m", "262144",
        "-L",
    ]
}

fn vocals_args() -> Vec<&'static str> {
    vec![
        "-c", "5", "chrt", "-f", "80", "pw-jack", "scsynth", "-u", "57111", "-H", "vocals", "-S",
        "48000", "-z", "64", "-i", "2", "-o", "2", "-a", "64", "-m", "65536", "-L",
    ]
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    let log: LogBuf = Arc::new(Mutex::new(Vec::new()));

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
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        ui.on_boot_orbits(move || {
            tokio::spawn(boot_scsynth(
                "orbits",
                "57110",
                orbits_args(),
                log.clone(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        ui.on_boot_vocals(move || {
            tokio::spawn(boot_scsynth(
                "vocals",
                "57111",
                vocals_args(),
                log.clone(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        ui.on_panic_orbits(move || {
            push_log(
                &log,
                &ui_weak,
                "[orbits] panic triggered: ending port 57110".into(),
            );
            kill_server("57110");
        });
    }

    {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        ui.on_panic_vocals(move || {
            push_log(
                &log,
                &ui_weak,
                "[vocals] panic triggered: ending port 57111".into(),
            );
            kill_server("57111");
        });
    }

    {
        let log = log.clone();
        let ui_weak = ui_weak.clone();
        ui.on_force_restart_all(move || {
            push_log(
                &log,
                &ui_weak,
                "[system] force restart: clearing all scsynth instances".into(),
            );
            let _ = Command::new("pkill").arg("-f").arg("scsynth").status();
        });
    }

    ui.run()
}
