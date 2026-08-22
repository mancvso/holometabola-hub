use slint::{Color, ComponentHandle};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

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
    kill_server(ui_weak, ORBITS_PORT);
    // The boot file arrives on stdin, so the port is what identifies our sclang.
    let pattern = format!("sclang -u {}", SCLANG_PORT);
    log_line(ui_weak, format!("$ pkill -f \"{pattern}\""));
    let _ = Command::new("pkill").args(["-f", &pattern]).status();
}

// A child process we keep talking to: its stdin, parked for later writes.
type ProcIn = Arc<Mutex<Option<tokio::process::ChildStdin>>>;

// Every child goes through here. stdin is always piped -- both so we can send
// it commands, and so it never inherits (and blocks on) the app's own terminal.
// stdout/stderr are streamed into the log.
async fn spawn_proc(
    name: &'static str,
    args: Vec<String>,
    init: Option<&'static str>,
    slot: ProcIn,
    ui_weak: slint::Weak<AppWindow>,
) {
    // Held across the spawn so a double-click cannot start a second copy.
    let mut guard = slot.lock().await;
    if guard.is_some() {
        log_line(&ui_weak, format!("[{name}] already running"));
        return;
    }

    log_line(&ui_weak, format!("[{name}] $ {}", args.join(" ")));

    let Some((program, rest)) = args.split_first() else {
        log_line(&ui_weak, format!("[{name}] no command given"));
        return;
    };

    let mut child = match tokio::process::Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log_line(&ui_weak, format!("[{name}] failed to launch: {e}"));
            return;
        }
    };

    if let Some(stdin) = child.stdin.take() {
        *guard = Some(stdin);
    }

    // Written while the lock is still held so nothing can slip a command in
    // ahead of the boot script.
    if let (Some(code), Some(stdin)) = (init, guard.as_mut()) {
        log_line(&ui_weak, format!("[{name}] > {code}"));
        if let Err(e) = stdin.write_all(format!("{code}\n").as_bytes()).await {
            log_line(&ui_weak, format!("[{name}] boot write failed: {e}"));
        }
        let _ = stdin.flush().await;
    }
    drop(guard);

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
        *slot.lock().await = None;
        log_line(&ui_weak, format!("[{name}] exited"));
    });
}

// Same as spawn_proc, plus a guard against an audio server already on that port.
async fn boot_server(
    name: &'static str,
    port: &'static str,
    args: Vec<String>,
    init: Option<&'static str>,
    slot: ProcIn,
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
    spawn_proc(name, args, init, slot, ui_weak).await;
}

// Write one line into a running child's stdin.
async fn send_line(
    name: &'static str,
    slot: ProcIn,
    ui_weak: slint::Weak<AppWindow>,
    code: String,
) {
    let mut guard = slot.lock().await;
    let Some(stdin) = guard.as_mut() else {
        log_line(&ui_weak, format!("[{name}] not running -- boot it first"));
        return;
    };

    log_line(&ui_weak, format!("[{name}] > {code}"));
    if let Err(e) = stdin.write_all(format!("{code}\n").as_bytes()).await {
        log_line(&ui_weak, format!("[{name}] write failed: {e}"));
        return;
    }
    if let Err(e) = stdin.flush().await {
        log_line(&ui_weak, format!("[{name}] flush failed: {e}"));
    }
}

// Editor relay. The lite-xl plugin already decomposes a block into `:{`, the
// block's lines, then `:}` -- one newline-terminated line per write -- and ghci
// does the multi-line accumulation itself. So this is a pure line relay: it
// never parses Haskell and never needs to know where a block begins or ends.
//
// Loopback only for now. If the app ever moves to the headless box while the
// editor stays on the laptop, this needs the same treatment as ~hubScsynthBind.
const EDITOR_PORT: u16 = 6140;

async fn serve_editor(tidal_in: ProcIn, ui_weak: slint::Weak<AppWindow>) {
    let addr = format!("127.0.0.1:{EDITOR_PORT}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            log_line(&ui_weak, format!("[edit] cannot bind {addr}: {e}"));
            return;
        }
    };
    log_line(&ui_weak, format!("[edit] listening on {addr}"));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log_line(&ui_weak, format!("[edit] accept failed: {e}"));
                continue;
            }
        };
        log_line(&ui_weak, format!("[edit] connected: {peer}"));

        let tidal_in = tidal_in.clone();
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            let mut count = 0usize;
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        count += 1;
                        // Logged verbatim by send_line, so an unbalanced `:{`
                        // from a half-flushed block is visible in the log
                        // rather than showing up as ghci silently swallowing
                        // the next block into the previous one.
                        send_line("tidal", tidal_in.clone(), ui_weak.clone(), line).await;
                    }
                    Ok(None) => {
                        log_line(
                            &ui_weak,
                            format!("[edit] {peer} disconnected after {count} lines"),
                        );
                        break;
                    }
                    Err(e) => {
                        log_line(&ui_weak, format!("[edit] read error from {peer}: {e}"));
                        break;
                    }
                }
            }
        });
    }
}

// The boot file is fed to sclang over stdin rather than passed as an argv file.
// Given a file argument sclang runs it and then never reads stdin at all (the
// trailing `-` in `sclang [options] [file..] [-]` does not change this), which
// would leave no way to talk to the running interpreter afterwards. Sending it
// as a .load means one channel both boots the engine and takes later commands.
const ORBITS_BOOT_CMD: &str = "\"/home/endo/Studio/Hub/startup.scd\".load;";

// Ports are held out of the SuperCollider/SuperDirt defaults so a stray sclang
// can never take a socket this stack depends on. These must stay in sync with
// Hub/startup.scd and Hub/BootTidal.hs -- see the header comments in both.
const ORBITS_PORT: &str = "6110"; // scsynth; equals Tidal's oBusPort
const VOCALS_PORT: &str = "6111"; // second, independent scsynth
const SCLANG_PORT: &str = "6120"; // sclang langPort; SuperDirt's /n_end responder binds here

// startup.scd sets s.options.device = "orbits", and the direct scsynth boot
// passes -H orbits, so the JACK client name is the same in both boot paths.
const ORBITS_JACK_CLIENT: &str = "orbits";
const ONYX_CLIENT: &str = "Onyx Artist 1-2 Pro";

// Two stereo channels per orbit. Must match ~hubOutChans in startup.scd, which
// derives it as (~hubGroups.size * ~hubPerGroup * 2) -- 36 today, 72 once the
// rig grows to 36 orbits across 4 groups. Both the CONNECT wiring and the
// direct scsynth boot's -o read from here.
const ORBITS_OUT_CHANNELS: u32 = 36;

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn orbits_connect_pairs() -> Vec<(String, String)> {
    (1..=ORBITS_OUT_CHANNELS)
        .map(|i| {
            let src = format!("{ORBITS_JACK_CLIENT}:out_{i}");
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

// Direct scsynth boot. These flags are the server's ONLY configuration -- no
// .scd can change them afterwards -- so they mirror the s.options block in
// Hub/startup.scd. A client that allocates against larger limits than the
// server actually has will get "Node/Group/SynthDef not found" back.
fn orbits_scsynth_args() -> Vec<String> {
    let out_chans = ORBITS_OUT_CHANNELS.to_string();
    // Same formula as startup.scd: hardware outs, plus private synth/dry/
    // globalEffect busses per orbit, plus headroom. Has to grow with the
    // orbit count rather than sit at a constant.
    let orbits = ORBITS_OUT_CHANNELS / 2;
    let audio_bus = (ORBITS_OUT_CHANNELS + (orbits * 8) + 128)
        .max(1024)
        .to_string();

    owned(&[
        "taskset",
        "-c",
        "4",
        "chrt",
        "-f",
        "80",
        "pw-jack",
        "-p",
        "128",
        "scsynth",
        "-u",
        ORBITS_PORT,
        "-H",
        ORBITS_JACK_CLIENT,
        "-S",
        "48000",
        "-z",
        "128",
        "-Z",
        "128",
        "-i",
        "0",
        "-o",
        &out_chans,
        "-a",
        &audio_bus,
        "-b",
        "524288",
        "-n",
        "524288",
        "-w",
        "256",
        "-m",
        "524288",
        "-l",
        "3",
        "-L",
    ])
}

// sclang boot: sclang spawns scsynth itself using the s.options in the boot
// file. -u pins the language port, which SuperDirt uses for its /n_end
// responder -- the only thing that clears its `flotsam` node dictionary.
// No file argument here on purpose: see ORBITS_BOOT_CMD.
//
// Deliberately NOT pinned or given realtime priority. A child inherits both,
// so pinning sclang put sclang and scsynth on one core at SCHED_FIFO 80 --
// and under FIFO an equal-priority thread never preempts a running one. While
// sclang blasted ~2500 /b_allocRead messages, scsynth could not be scheduled
// to drain its UDP socket, so the kernel silently dropped them (and initTree's
// /g_new with them). startup.scd pins scsynth alone via Server.program.
fn orbits_sclang_args() -> Vec<String> {
    owned(&["sclang", "-u", SCLANG_PORT])
}

fn vocals_args() -> Vec<String> {
    owned(&[
        "taskset",
        "-c",
        "5",
        "chrt",
        "-f",
        "80",
        "pw-jack",
        "-p",
        "64",
        "scsynth",
        "-u",
        VOCALS_PORT,
        "-H",
        "vocals",
        "-S",
        "48000",
        "-z",
        "64",
        "-i",
        "2",
        "-o",
        "2",
        "-a",
        "64",
        "-m",
        "65536",
        "-L",
    ])
}

const TIDAL_BOOT_FILE: &str = "/home/endo/Studio/Hub/BootTidal.hs";

// ghci gets its own core, clear of the audio cores (4 = orbits, 5 = vocals).
// Deliberately no `chrt -f` here: giving a garbage-collected runtime realtime
// FIFO priority lets a GC pause starve everything scheduled below it.
fn tidal_args() -> Vec<String> {
    owned(&[
        "taskset",
        "-c",
        "6",
        "ghci",
        "-ghci-script",
        TIDAL_BOOT_FILE,
    ])
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    let orbits_in: ProcIn = Arc::new(Mutex::new(None));
    let vocals_in: ProcIn = Arc::new(Mutex::new(None));
    let tidal_in: ProcIn = Arc::new(Mutex::new(None));

    // Editor relay, up for the life of the app -- independent of whether Tidal
    // is booted, so connecting early just reports "not running" per line
    // instead of refusing the connection.
    {
        let tidal_in = tidal_in.clone();
        let ui_weak = ui_weak.clone();
        tokio::spawn(serve_editor(tidal_in, ui_weak));
    }

    // Spawn async telemetry engine checking scsynth status
    {
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            loop {
                let orbits_running = is_process_running(ORBITS_PORT);
                let vocals_running = is_process_running(VOCALS_PORT);

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
        let orbits_in = orbits_in.clone();
        ui.on_boot_orbits_scsynth(move || {
            tokio::spawn(boot_server(
                "orbits",
                ORBITS_PORT,
                orbits_scsynth_args(),
                None,
                orbits_in.clone(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_boot_orbits_sclang(move || {
            tokio::spawn(boot_server(
                "orbits",
                ORBITS_PORT,
                orbits_sclang_args(),
                Some(ORBITS_BOOT_CMD),
                orbits_in.clone(),
                ui_weak.clone(),
            ));
        });
    }

    // Ask sclang what the server's node tree actually looks like. Prints to
    // sclang's stdout, which lands in this log.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_orbits_dump_tree(move || {
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "s.queryAllNodes;".to_string(),
            ));
        });
    }

    // Re-run the boot-time tree init by hand: creates the default group and
    // re-runs ServerTree, which is what rebuilds SuperDirt's orbit groups.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_orbits_init_tree(move || {
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "s.initTree;".to_string(),
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
        let vocals_in = vocals_in.clone();
        ui.on_boot_vocals(move || {
            tokio::spawn(boot_server(
                "vocals",
                VOCALS_PORT,
                vocals_args(),
                None,
                vocals_in.clone(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_boot_tidal(move || {
            tokio::spawn(spawn_proc(
                "tidal",
                tidal_args(),
                None,
                tidal_in.clone(),
                ui_weak.clone(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_tidal_test_pattern(move || {
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                r#"b2 $ s "bd(3, 4)" "#.to_string(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_tidal_hush(move || {
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                "hush".to_string(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_stop_tidal(move || {
            // :quit lets ghci shut down cleanly; the wait task clears the handle.
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                ":quit".to_string(),
            ));
        });
    }

    // Recording. The label lands in both an sclang string literal and a
    // filesystem path, so keep it to characters that are safe in both.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_rec_start(move || {
            let label = ui_weak
                .upgrade()
                .map(|ui| ui.get_session_name().to_string())
                .unwrap_or_default();
            let label: String = label
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let label = if label.is_empty() {
                "session".to_string()
            } else {
                label
            };
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                format!("~hubRecStart.value(\"{label}\");"),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_rec_stop(move || {
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "~hubRecStop.value;".to_string(),
            ));
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_orbits(move || {
            log_line(
                &ui_weak,
                format!("[orbits] panic triggered: ending port {ORBITS_PORT}"),
            );
            kill_orbits(&ui_weak);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_vocals(move || {
            log_line(
                &ui_weak,
                format!("[vocals] panic triggered: ending port {VOCALS_PORT}"),
            );
            kill_server(&ui_weak, VOCALS_PORT);
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
