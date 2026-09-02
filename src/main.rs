mod midi;
mod recordings;
mod labels;
mod sections;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

slint::include_modules!();

static LATES_COUNT: AtomicU32 = AtomicU32::new(0);

// PipeWire quantum selected in the UI. Written to clock.force-quantum on
// APPLY, and read back by orbits_scsynth_args() for the NEXT scsynth boot
// (pw-jack -p sets node.force-quantum, which overrides the global -- so a
// running server keeps its old quantum until rebooted).
static QUANTUM: AtomicU32 = AtomicU32::new(512);
const QUANTUM_SIZES: [u32; 7] = [32, 64, 128, 256, 512, 1024, 2048];

// Recording live state, driven by the "HUB REC:" lines sclang prints
// (see recording.scd) rather than by the button clicks.
static REC_ACTIVE: AtomicBool = AtomicBool::new(false);
static REC_TICKER: AtomicBool = AtomicBool::new(false);

// Wayland identifies a window by its app_id, and the shell matches that against
// a desktop entry to decide what a taskbar click does. Slint never calls
// set_app_id, and winit only does so when `platform_specific.name` is set, so
// by default this window ships with no identity at all: COSMIC cannot pair the
// taskbar button with the window, and clicking it toggles rather than raises.
//
// Must match the basename of packaging/live-audio-control.desktop, which is
// what the shell looks up to find the icon and name.
const APP_ID: &str = "live-audio-control";

// Has to run before the first window exists, since the hook is consulted at
// window construction. A failure here costs only the taskbar behaviour, so it
// warns rather than aborting the boot.
fn install_backend() {
    use i_slint_backend_winit::winit::platform::wayland::WindowAttributesExtWayland;

    match i_slint_backend_winit::Backend::builder()
        .with_window_attributes_hook(|attrs| attrs.with_name(APP_ID, APP_ID))
        .build()
    {
        Ok(backend) => {
            if let Err(e) = slint::platform::set_platform(Box::new(backend)) {
                eprintln!("[ui] could not install winit backend ({e:?}); app_id unset");
            }
        }
        Err(e) => eprintln!("[ui] winit backend build failed ({e}); app_id unset"),
    }
}

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

// ── chain node state ────────────────────────────────────────────────
// A node is Off until its step starts, Running while in flight, and
// Ready once its readiness evidence arrived: a stdout marker for sclang
// ("HUB: scsynth=") and tidal ("Connected to SuperDirt."), our own
// completion for connect and pin, pgrep for the bare vocals scsynth.

#[derive(Clone, Copy, PartialEq)]
enum NodeId {
    Sclang,
    Connect,
    Pin,
    Tidal,
    Vocals,
}

fn set_node_state(ui_weak: &slint::Weak<AppWindow>, node: NodeId, state: NodeState) {
    let uw = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = uw.upgrade() {
            match node {
                NodeId::Sclang => ui.set_sclang_node(state),
                NodeId::Connect => ui.set_connect_node(state),
                NodeId::Pin => ui.set_pin_node(state),
                NodeId::Tidal => ui.set_tidal_node(state),
                NodeId::Vocals => ui.set_vocals_node(state),
            }
        }
    });
}

// ── ArtButton working / done state ────────────────────────────────
// Each ArtButton has a `working` (in-progress, pulsing) and `done`
// (sticky blue dot) flag on the AppWindow root. `btn_press` clears
// done and sets working on press; `btn_done` clears working and sets
// done on completion. Both run on the UI thread via invoke_from_event_loop
// so they are safe to call from a blocking task or a spawned future.
// The setters are passed as plain fn pointers to the generated
// AppWindow methods, keeping the call sites one line each.
type BoolSetter = fn(&AppWindow, bool);

fn btn_press(ui_weak: &slint::Weak<AppWindow>, set_working: BoolSetter, set_done: BoolSetter) {
    let uw = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = uw.upgrade() {
            set_done(&ui, false);      // clear sticky done from the previous press
            set_working(&ui, true);
        }
    });
}

fn btn_done(ui_weak: &slint::Weak<AppWindow>, set_working: BoolSetter, set_done: BoolSetter) {
    let uw = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = uw.upgrade() {
            set_working(&ui, false);
            set_done(&ui, true);       // sticky: stays until the next press
        }
    });
}

// Boot order is strict (see README), so a fresh sclang invalidates every
// step after it. A rebooted stack also invalidates a live Tidal: it
// handshakes once and keeps writing the dead instance's busses.
fn mark_spawned(ui_weak: &slint::Weak<AppWindow>, node: Option<NodeId>) {
    match node {
        Some(NodeId::Sclang) => {
            set_node_state(ui_weak, NodeId::Sclang, NodeState::Running);
            for n in [NodeId::Connect, NodeId::Pin, NodeId::Tidal] {
                set_node_state(ui_weak, n, NodeState::Off);
            }
            log_line(
                ui_weak,
                "[chain] fresh sclang: CONNECT and PIN must run again; restart Tidal".into(),
            );
        }
        Some(NodeId::Tidal) => set_node_state(ui_weak, NodeId::Tidal, NodeState::Running),
        Some(NodeId::Vocals) => set_node_state(ui_weak, NodeId::Vocals, NodeState::Running),
        _ => {}
    }
}

fn mark_exited(ui_weak: &slint::Weak<AppWindow>, node: Option<NodeId>) {
    match node {
        Some(NodeId::Sclang) => {
            for n in [NodeId::Sclang, NodeId::Connect, NodeId::Pin, NodeId::Tidal] {
                set_node_state(ui_weak, n, NodeState::Off);
            }
            log_line(
                ui_weak,
                "[chain] sclang gone: boot the chain again, tidal included".into(),
            );
        }
        Some(NodeId::Tidal) => set_node_state(ui_weak, NodeId::Tidal, NodeState::Off),
        Some(NodeId::Vocals) => set_node_state(ui_weak, NodeId::Vocals, NodeState::Off),
        _ => {}
    }
}

// Asynchronously inspect active ports via native Linux commands.
//
// The pattern has to tolerate anything between "scsynth" and "-u <port>": the
// sclang boot path goes through Server.program, which inserts `-B <addr>`
// there, so a literal "scsynth -u <port>" only ever matched the direct boot
// and reported CRASHED for a perfectly healthy sclang-booted server.
fn is_process_running(port: &str) -> bool {
    let output = Command::new("pgrep")
        .args(["-f", &format!("scsynth.*-u {}", port)])
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

// Per-thread pinning for the orbits server.
//
// scsynth renders everything on a single thread -- the PipeWire client
// callback, named data-loop.0 -- while its OSC receive, NRT command and disk
// threads wake in bursts whenever Tidal fires a section change. `taskset -c 4`
// on the process puts all eight on one logical CPU, so those bursts preempt
// the audio callback: measured at up to 346 involuntary context switches per
// second during section changes, against 41 on a free-floating server.
//
// Splitting them leaves the callback alone on the isolated CPU and moves the
// rest to its SMT sibling, which isolcpus also holds out of the scheduler.
// Affinity on our own children needs no privileges -- unlike the SCHED_FIFO
// lease, which still has to come from outside.
const ORBITS_DSP_CPU: &str = "4"; // isolated; the audio callback, alone
const ORBITS_AUX_CPU: &str = "10"; // isolated SMT sibling; every other thread
const ORBITS_DSP_THREAD: &str = "data-loop.0";

// The sclang boot path goes through Server.program, which inserts `-B <addr>`
// ahead of `-u <port>`, so a literal "scsynth -u <port>" pattern never matches
// it. Allow anything between the two.
fn orbits_scsynth_pid() -> Option<u32> {
    let out = Command::new("pgrep")
        .args(["-f", &format!("scsynth.*-u {}", ORBITS_PORT)])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
}

fn pin_orbits_threads(ui_weak: slint::Weak<AppWindow>) {
    set_node_state(&ui_weak, NodeId::Pin, NodeState::Running);

    let Some(pid) = orbits_scsynth_pid() else {
        log_line(&ui_weak, "[pin] no orbits scsynth running".into());
        set_node_state(&ui_weak, NodeId::Pin, NodeState::Off);
        return;
    };

    let task_dir = format!("/proc/{pid}/task");
    let entries = match std::fs::read_dir(&task_dir) {
        Ok(e) => e,
        Err(e) => {
            log_line(&ui_weak, format!("[pin] cannot read {task_dir}: {e}"));
            set_node_state(&ui_weak, NodeId::Pin, NodeState::Off);
            return;
        }
    };

    let (mut dsp, mut aux, mut failed) = (0u32, 0u32, 0u32);
    for entry in entries.flatten() {
        let tid = entry.file_name().to_string_lossy().into_owned();
        let comm = std::fs::read_to_string(format!("{task_dir}/{tid}/comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        let is_dsp = comm == ORBITS_DSP_THREAD;
        let cpu = if is_dsp {
            ORBITS_DSP_CPU
        } else {
            ORBITS_AUX_CPU
        };

        let ok = Command::new("taskset")
            .args(["-cp", cpu, &tid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if ok {
            if is_dsp {
                dsp += 1;
                log_line(&ui_weak, format!("[pin] {comm} (tid {tid}) -> cpu {cpu}"));
            } else {
                aux += 1;
            }
        } else {
            failed += 1;
        }
    }

    if dsp == 0 {
        // No data-loop.0 means scsynth has not attached to PipeWire yet, so
        // every thread just went to the aux CPU and the DSP core sits empty.
        log_line(
            &ui_weak,
            format!("[pin] WARNING: no {ORBITS_DSP_THREAD} thread -- is the server connected?"),
        );
    }
    log_line(
        &ui_weak,
        format!(
            "[pin] scsynth {pid}: {dsp} dsp on cpu {ORBITS_DSP_CPU}, \
             {aux} aux on cpu {ORBITS_AUX_CPU}{}",
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            }
        ),
    );
    set_node_state(
        &ui_weak,
        NodeId::Pin,
        if dsp >= 1 {
            NodeState::Ready
        } else {
            NodeState::Off
        },
    );
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

// stdout line that proves a component is ready.
//   sclang -> "HUB: scsynth="        (last postln of startup.scd's waitForBoot)
//   tidal  -> "Connected to SuperDirt."  (Target.hs handshake, cVerbose on)
type ReadyMarker = (&'static str, NodeId);

// Watches one stream of a child, forwarding every line to the log. When a
// readiness marker is configured, the first line containing it promotes the
// node to Ready -- once per process instance.
async fn watch_lines(
    name: &'static str,
    mut lines: tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    ui_weak: slint::Weak<AppWindow>,
    ready: Option<ReadyMarker>,
    ready_seen: Arc<AtomicBool>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some((marker, node)) = ready {
            if line.contains(marker) && !ready_seen.swap(true, Ordering::SeqCst) {
                set_node_state(&ui_weak, node, NodeState::Ready);
            }
        }
        if name == "orbits" {
            handle_rec_line(&line, &ui_weak);
        }
        log_line(&ui_weak, format!("[{name}] {line}"));
    }
}

// Every child goes through here. stdin is always piped -- both so we can send
// it commands, and so it never inherits (and blocks on) the app's own terminal.
// stdout/stderr are streamed into the log.
async fn spawn_proc(
    name: &'static str,
    args: Vec<String>,
    init: Option<&'static str>,
    slot: ProcIn,
    ready: Option<ReadyMarker>,
    node: Option<NodeId>,
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

    mark_spawned(&ui_weak, node);

    let ready_seen = Arc::new(AtomicBool::new(false));

    if let Some(stdout) = child.stdout.take() {
        let lines = BufReader::new(stdout).lines();
        let ui_weak = ui_weak.clone();
        let seen = ready_seen.clone();
        tokio::spawn(watch_lines(name, lines, ui_weak, ready, seen));
    }

    if let Some(stderr) = child.stderr.take() {
        let lines = BufReader::new(stderr).lines();
        let ui_weak = ui_weak.clone();
        let seen = ready_seen.clone();
        tokio::spawn(watch_lines(name, lines, ui_weak, ready, seen));
    }

    tokio::spawn(async move {
        let _ = child.wait().await;
        *slot.lock().await = None;
        mark_exited(&ui_weak, node);
        log_line(&ui_weak, format!("[{name}] exited"));
    });
}

// Same as spawn_proc, plus a guard against an audio server already on that port.
#[allow(clippy::too_many_arguments)]
async fn boot_server(
    name: &'static str,
    port: &'static str,
    args: Vec<String>,
    init: Option<&'static str>,
    slot: ProcIn,
    ready: Option<ReadyMarker>,
    node: Option<NodeId>,
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
    spawn_proc(name, args, init, slot, ready, node, ui_weak).await;
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

// ── recording live state ─────────────────────────────────────────────
// recording.scd announces itself on sclang's stdout:
//   "HUB REC: recording 18 stems -> /path"
//   "HUB REC: stopped -> /path"

fn set_rec(ui_weak: &slint::Weak<AppWindow>, active: bool, elapsed: Option<&str>, path: Option<&str>) {
    let uw = ui_weak.clone();
    let elapsed = elapsed.map(SharedString::from);
    let path = path.map(SharedString::from);
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = uw.upgrade() {
            ui.set_rec_active(active);
            if let Some(e) = elapsed {
                ui.set_rec_elapsed(e);
            }
            if let Some(p) = path {
                ui.set_rec_path(p);
            }
        }
    });
}

fn rec_start_ticker(ui_weak: &slint::Weak<AppWindow>) {
    if REC_TICKER.swap(true, Ordering::SeqCst) {
        return; // already counting
    }
    let uw = ui_weak.clone();
    tokio::spawn(async move {
        let mut secs = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !REC_ACTIVE.load(Ordering::SeqCst) {
                break;
            }
            secs += 1;
            let s = format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
            let uw2 = uw.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw2.upgrade() {
                    ui.set_rec_elapsed(s.into());
                }
            });
        }
        REC_TICKER.store(false, Ordering::SeqCst);
    });
}

fn handle_rec_line(line: &str, ui_weak: &slint::Weak<AppWindow>) {
    if line.contains("HUB REC: recording") {
        REC_ACTIVE.store(true, Ordering::SeqCst);
        let path = line.rsplit("-> ").next().unwrap_or_default().trim();
        set_rec(
            ui_weak,
            true,
            Some(labels::REC_STARTING_ELAPSED),
            if path.is_empty() { None } else { Some(path) },
        );
        rec_start_ticker(ui_weak);
    } else if line.contains("HUB REC: stopped") {
        REC_ACTIVE.store(false, Ordering::SeqCst);
        set_rec(ui_weak, false, Some(labels::REC_STARTING_ELAPSED), None);
    }
}

// ── editor relay ─────────────────────────────────────────────────────
// Editor relay. The lite-xl plugin already decomposes a block into `:{`, the
// block's lines, then `:}` -- one newline-terminated line per write -- and ghci
// does the multi-line accumulation itself. So this is a pure line relay: it
// never parses Haskell and never needs to know where a block begins or ends.
//
// Loopback only for now. If the app ever moves to the headless box while the
// editor stays on the laptop, this needs the same treatment as ~hubScsynthBind.
//
// The listener can be stopped and restarted from the UI. A stop closes the
// listening socket (no new connections); connections already open drain out
// on their own -- the song runner and benchmark open one short-lived
// connection per run, so nothing lingers in practice.
const EDITOR_PORT: u16 = 6140;

// (generation, shutdown sender). The generation keeps a stale task's cleanup
// from wiping the entry of a relay that was restarted in the meantime.
type RelayCtl = Arc<Mutex<Option<(u64, oneshot::Sender<()>)>>>;

async fn serve_editor(
    gen: u64,
    tidal_in: ProcIn,
    ui_weak: slint::Weak<AppWindow>,
    mut shutdown: oneshot::Receiver<()>,
    ctl: RelayCtl,
) {
    let addr = format!("127.0.0.1:{EDITOR_PORT}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            log_line(&ui_weak, format!("[edit] cannot bind {addr}: {e}"));
            clear_relay(&ctl, gen).await;
            return;
        }
    };

    set_relay_up(&ui_weak, true);
    log_line(&ui_weak, format!("[edit] listening on {addr}"));

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log_line(&ui_weak, "[edit] relay stopped".into());
                break;
            }
            pair = listener.accept() => {
                let (stream, peer) = match pair {
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
    }

    clear_relay(&ctl, gen).await;
    set_relay_up(&ui_weak, false);
}

fn set_relay_up(ui_weak: &slint::Weak<AppWindow>, up: bool) {
    let uw = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = uw.upgrade() {
            ui.set_relay_up(up);
        }
    });
}

async fn clear_relay(ctl: &RelayCtl, gen: u64) {
    let mut g = ctl.lock().await;
    if g.as_ref().is_some_and(|(cur, _)| *cur == gen) {
        *g = None;
    }
}

async fn relay_start(ctl: &RelayCtl, tidal_in: &ProcIn, ui_weak: &slint::Weak<AppWindow>) {
    let mut g = ctl.lock().await;
    if g.is_some() {
        log_line(ui_weak, "[edit] relay already up".into());
        return;
    }
    static RELAY_GEN: AtomicU64 = AtomicU64::new(0);
    let gen = RELAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = oneshot::channel();
    *g = Some((gen, tx));
    drop(g);
    tokio::spawn(serve_editor(gen, tidal_in.clone(), ui_weak.clone(), rx, ctl.clone()));
}

async fn relay_stop(ctl: &RelayCtl, ui_weak: &slint::Weak<AppWindow>) {
    match ctl.lock().await.take() {
        Some((_, tx)) => {
            let _ = tx.send(());
        }
        None => log_line(ui_weak, "[edit] relay not running".into()),
    }
}

async fn relay_restart(ctl: &RelayCtl, tidal_in: &ProcIn, ui_weak: &slint::Weak<AppWindow>) {
    relay_stop(ctl, ui_weak).await;
    // Give the old listener a beat to release the port before rebinding.
    tokio::time::sleep(Duration::from_millis(250)).await;
    relay_start(ctl, tidal_in, ui_weak).await;
}

// ── quantum ──────────────────────────────────────────────────────────

fn read_current_quantum() -> Option<u32> {
    let out = Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.force-quantum"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Tolerates both "clock.force-quantum = 512" and a bare "512".
    text.split_whitespace().last()?.parse().ok()
}

fn apply_quantum(ui_weak: &slint::Weak<AppWindow>, size: u32) {
    QUANTUM.store(size, Ordering::SeqCst);
    let uw = ui_weak.clone();
    tokio::task::spawn_blocking(move || {
        let status = Command::new("pw-metadata")
            .args(["-n", "settings", "0", "clock.force-quantum", &size.to_string()])
            .status();
        match status {
            Ok(s) if s.success() => {
                log_line(
                    &uw,
                    format!("[quantum] clock.force-quantum -> {size} (scsynth picks it up on next boot)"),
                );
            }
            Ok(s) => log_line(&uw, format!("[quantum] pw-metadata exited with {s}")),
            Err(e) => log_line(&uw, format!("[quantum] pw-metadata failed: {e}")),
        }
    });
}

// ── boot config ──────────────────────────────────────────────────────

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

// Orbit mute grid. Must match ~hubGroups / ~hubPerGroup in startup.scd and
// the b1../l1../a1.. aliases in BootTidal.hs. Growing to 8 per group is an
// edit here plus those two files.
const MUTE_GROUP_SLUGS: [char; 3] = ['b', 'l', 'a'];
const ORBITS_PER_GROUP: usize = 6;

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
    set_node_state(&ui_weak, NodeId::Connect, NodeState::Running);
    let pairs = orbits_connect_pairs();
    let total = pairs.len();
    let mut ok = 0usize;

    for (src, dst) in pairs {
        log_line(&ui_weak, format!("$ jack_connect {src} \"{dst}\""));
        match Command::new("jack_connect").arg(&src).arg(&dst).status() {
            Ok(status) if status.success() => {
                ok += 1;
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

    if ok == total {
        log_line(&ui_weak, format!("[orbits] connect complete ({ok}/{total})"));
        set_node_state(&ui_weak, NodeId::Connect, NodeState::Ready);
    } else {
        log_line(&ui_weak, format!("[orbits] connect incomplete ({ok}/{total})"));
        set_node_state(&ui_weak, NodeId::Connect, NodeState::Off);
    }
}

// Direct scsynth boot. These flags are the server's ONLY configuration -- no
// .scd can change them afterwards -- so they mirror the s.options block in
// Hub/startup.scd. A client that allocates against larger limits than the
// server actually has will get "Node/Group/SynthDef not found" back.
//
// -p, -z and -Z follow the quantum selected in the UI. pw-jack -p sets
// node.force-quantum, which overrides the global clock.force-quantum, so this
// is the only way the selection actually reaches scsynth.
fn orbits_scsynth_args() -> Vec<String> {
    let quantum = QUANTUM.load(Ordering::SeqCst).to_string();
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
        &quantum,
        "scsynth",
        "-u",
        ORBITS_PORT,
        "-H",
        ORBITS_JACK_CLIENT,
        "-S",
        "48000",
        "-z",
        &quantum,
        "-Z",
        &quantum,
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
// Realtime, but deliberately BELOW scsynth, and deliberately NOT pinned.
//
// The original incident this guards against: pinning sclang put sclang and
// scsynth on one core at SCHED_FIFO 80, and under FIFO an equal-priority
// thread never preempts a running one. While sclang blasted ~2500 /b_allocRead
// messages, scsynth could not be scheduled to drain its UDP socket, so the
// kernel silently dropped them (and initTree's /g_new with them). Both halves
// of that fix still hold: no affinity mask here, and a priority strictly under
// the server's, so scsynth can always preempt sclang no matter which
// housekeeping core sclang lands on.
//
// chrt needs no privileges -- the user is in @audio, which grants rtprio 95
// via /etc/security/limits.d/audio.conf. It only *persists* once sclang is in
// system76-scheduler's exceptions list: its 60s refresh resets SCHED_FIFO to
// SCHED_OTHER on any process it manages. See
// packaging/system76-scheduler-config.kdl.
const SCLANG_RT_PRIO: &str = "73"; // must stay below scsynth's 80

fn orbits_sclang_args() -> Vec<String> {
    owned(&["chrt", "-f", SCLANG_RT_PRIO, "sclang", "-u", SCLANG_PORT])
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

// ── UI models ────────────────────────────────────────────────────────

fn midi_model() -> ModelRc<MidiRow> {
    let rows: Vec<MidiRow> = midi::list_devices()
        .into_iter()
        .map(|d| MidiRow {
            id: d.id.into(),
            name: d.name.into(),
            direction: d.direction.into(),
        })
        .collect();
    ModelRc::new(VecModel::from_slice(&rows))
}

/// Build the mute group model. Group names come from labels; the
/// accent color is chosen by group index on the Slint side, so no
/// color ever crosses into Rust.
fn mute_model() -> ModelRc<MuteGroupRow> {
    let groups: Vec<MuteGroupRow> = MUTE_GROUP_SLUGS
        .iter()
        .enumerate()
        .map(|(gi, letter)| {
            let items: Vec<MuteItemRow> = (1..=ORBITS_PER_GROUP)
                .map(|i| MuteItemRow {
                    label: format!("{letter}{i}").into(),
                    muted: false,
                })
                .collect();
            MuteGroupRow {
                name: labels::MUTE_GROUP_NAMES[gi].into(),
                items: ModelRc::new(VecModel::from_slice(&items)),
            }
        })
        .collect();
    ModelRc::new(VecModel::from_slice(&groups))
}

type SessionPaths = Arc<std::sync::Mutex<Vec<PathBuf>>>;

fn sessions_model(paths: &SessionPaths) -> ModelRc<SessionRow> {
    let sessions = recordings::list_sessions();
    *paths.lock().unwrap() = sessions.iter().map(|s| s.path.clone()).collect();
    let rows: Vec<SessionRow> = sessions
        .into_iter()
        .map(|s| SessionRow {
            name: s.name.into(),
            info: format!("{} stems", s.stems).into(),
        })
        .collect();
    ModelRc::new(VecModel::from_slice(&rows))
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    install_backend();
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    let orbits_in: ProcIn = Arc::new(Mutex::new(None));
    let vocals_in: ProcIn = Arc::new(Mutex::new(None));
    let tidal_in: ProcIn = Arc::new(Mutex::new(None));
    let relay_ctl: RelayCtl = Arc::new(Mutex::new(None));
    let session_paths: SessionPaths = Arc::new(std::sync::Mutex::new(Vec::new()));

    // ── initial models ────────────────────────────────────────────
    ui.set_quantum_sizes(ModelRc::new(VecModel::from_slice(
        &QUANTUM_SIZES
            .iter()
            .map(|q| SharedString::from(q.to_string()))
            .collect::<Vec<_>>(),
    )));
    if let Some(q) = read_current_quantum() {
        if let Some(idx) = QUANTUM_SIZES.iter().position(|s| *s == q) {
            ui.set_quantum_index(idx as i32);
            QUANTUM.store(q, Ordering::SeqCst);
        }
    }
    ui.set_mute_groups(mute_model());
    ui.set_midi_rows(midi_model());
    ui.set_sessions(sessions_model(&session_paths));

    // ── song timeline ──────────────────────────────────────────
    // Populate the dropdown with every song folder under runs/.
    // No .sections.json is read here -- that only happens when the
    // user picks a song and presses APPLY (on_song_apply below), so
    // a folder that has not been benched yet is listed but loads
    // empty rather than failing.
    ui.set_song_options(ModelRc::new(VecModel::from_slice(
        &sections::list_song_folders(std::path::Path::new(sections::RUNS_DIR))
            .iter()
            .map(|s| SharedString::from(s.as_str()))
            .collect::<Vec<_>>(),
    )));
    ui.set_song_index(0);
    ui.set_song_title("pick a song and press APPLY".into());

    // APPLY: parse the selected song folder's .sections.json and push
    // the parsed timeline to the SONG view. Runs on the tokio runtime
    // (parse_song_sections does blocking file I/O) and marshals the
    // resulting model back onto the UI thread. A missing or empty file
    // yields an empty timeline -- the title line reports which song
    // was selected and whether any sections were found.
    {
        let ui_weak = ui_weak.clone();
        ui.on_song_apply(move || {
            log_line(&ui_weak, "[song] APPLY pressed".into());
            btn_press(&ui_weak, AppWindow::set_song_apply_working, AppWindow::set_song_apply_done);
            // Read the selection on the UI thread: Slint property/model
            // access from a blocking thread panics (the ModelRc and the
            // generated getters are not Sync in practice), and that
            // panic is swallowed by spawn_blocking -- which is why the
            // handler went silent after "APPLY pressed". Only the
            // string crosses into the blocking task.
            let song = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let idx = ui.get_song_index();
                let m = ui.get_song_options();
                let n = m.row_count();
                (idx as usize)
                    .checked_rem(n.max(1))
                    .and_then(|i| m.row_data(i))
                    .map(|s| s.to_string())
            };
            let Some(song) = song else {
                log_line(&ui_weak, "[song] no song selected".into());
                btn_done(&ui_weak, AppWindow::set_song_apply_working, AppWindow::set_song_apply_done);
                return;
            };
            let ui_weak = ui_weak.clone();
            tokio::task::spawn_blocking(move || {
                let dir = std::path::Path::new(sections::RUNS_DIR).join(&song);
                log_line(&ui_weak, format!("[song] parsing {}", dir.display()));
                let parsed = sections::parse_song_sections(&dir);
                let duration = parsed.duration;
                let track_n: usize = parsed.groups.iter().map(|g| g.tracks.len()).sum();
                let title = if parsed.is_empty() {
                    format!("{song}: no .sections.json yet (run the bench)")
                } else {
                    format!("{} · {} tracks · {:.0}s", parsed.song, track_n, duration)
                };
                log_line(
                    &ui_weak,
                    format!("[song] {song}: {track_n} tracks, {:.0}s", duration),
                );
                // The Slint ModelRc holds an Rc and is !Send, so the
                // model is built on the UI thread from the Send parsed
                // data, not inside spawn_blocking.
                let ui_weak = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_song_groups(sections::to_group_model(&parsed));
                        ui.set_song_duration(duration);
                        ui.set_song_title(title.into());
                    }
                    btn_done(&ui_weak, AppWindow::set_song_apply_working, AppWindow::set_song_apply_done);
                });
            });
        });
    }

    // Editor relay, up for the life of the app -- independent of whether Tidal
    // is booted, so connecting early just reports "not running" per line
    // instead of refusing the connection. Stop/restart from the sidebar.
    {
        let ctl = relay_ctl.clone();
        let tidal_in = tidal_in.clone();
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            relay_start(&ctl, &tidal_in, &ui_weak).await;
        });
    }

    // Telemetry: process liveness for orbits/vocals. Also demotes the chain
    // nodes when the orbits server dies externally (crash, kill -9, PANIC).
    {
        let ui_weak = ui_weak.clone();
        tokio::spawn(async move {
            let (mut prev_orbits, mut prev_vocals) = (false, false);
            loop {
                let orbits_running = is_process_running(ORBITS_PORT);
                let vocals_running = is_process_running(VOCALS_PORT);

                let uw = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_orbits_online(orbits_running);
                        ui.set_vocals_online(vocals_running);
                    }
                });

                if orbits_running != prev_orbits {
                    if !orbits_running {
                        for n in [NodeId::Sclang, NodeId::Connect, NodeId::Pin, NodeId::Tidal] {
                            set_node_state(&ui_weak, n, NodeState::Off);
                        }
                    }
                    prev_orbits = orbits_running;
                }
                if vocals_running != prev_vocals {
                    set_node_state(
                        &ui_weak,
                        NodeId::Vocals,
                        if vocals_running {
                            NodeState::Ready
                        } else {
                            NodeState::Off
                        },
                    );
                    prev_vocals = vocals_running;
                }

                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    // ── boot chain callbacks ─────────────────────────────────────
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_boot_orbits_scsynth(move || {
            btn_press(&ui_weak, AppWindow::set_boot_orbits_scsynth_working, AppWindow::set_boot_orbits_scsynth_done);
            tokio::spawn(boot_server(
                "orbits",
                ORBITS_PORT,
                orbits_scsynth_args(),
                None,
                orbits_in.clone(),
                Some(("server ready", NodeId::Sclang)),
                Some(NodeId::Sclang),
                ui_weak.clone(),
            ));
            btn_done(&ui_weak, AppWindow::set_boot_orbits_scsynth_working, AppWindow::set_boot_orbits_scsynth_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_boot_orbits_sclang(move || {
            btn_press(&ui_weak, AppWindow::set_boot_orbits_sclang_working, AppWindow::set_boot_orbits_sclang_done);
            tokio::spawn(boot_server(
                "orbits",
                ORBITS_PORT,
                orbits_sclang_args(),
                Some(ORBITS_BOOT_CMD),
                orbits_in.clone(),
                Some(("HUB: scsynth=", NodeId::Sclang)),
                Some(NodeId::Sclang),
                ui_weak.clone(),
            ));
            btn_done(&ui_weak, AppWindow::set_boot_orbits_sclang_working, AppWindow::set_boot_orbits_sclang_done);
        });
    }

    // Ask sclang what the server's node tree actually looks like. Prints to
    // sclang's stdout, which lands in this log.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_orbits_dump_tree(move || {
            btn_press(&ui_weak, AppWindow::set_orbits_dump_tree_working, AppWindow::set_orbits_dump_tree_done);
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "s.queryAllNodes;".to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_orbits_dump_tree_working, AppWindow::set_orbits_dump_tree_done);
        });
    }

    // Re-run the boot-time tree init by hand: creates the default group and
    // re-runs ServerTree, which is what rebuilds SuperDirt's orbit groups.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_orbits_init_tree(move || {
            btn_press(&ui_weak, AppWindow::set_orbits_init_tree_working, AppWindow::set_orbits_init_tree_done);
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "s.initTree;".to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_orbits_init_tree_working, AppWindow::set_orbits_init_tree_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_connect_orbits(move || {
            btn_press(&ui_weak, AppWindow::set_connect_orbits_working, AppWindow::set_connect_orbits_done);
            let ui_weak = ui_weak.clone();
            tokio::task::spawn_blocking(move || {
                connect_orbits(ui_weak.clone());
                btn_done(&ui_weak, AppWindow::set_connect_orbits_working, AppWindow::set_connect_orbits_done);
            });
        });
    }

    // Press after the server is up and connected: the thread list only settles
    // once scsynth has attached to PipeWire and spawned data-loop.0.
    {
        let ui_weak = ui_weak.clone();
        ui.on_pin_orbits_threads(move || {
            btn_press(&ui_weak, AppWindow::set_pin_orbits_threads_working, AppWindow::set_pin_orbits_threads_done);
            let ui_weak = ui_weak.clone();
            tokio::task::spawn_blocking(move || {
                pin_orbits_threads(ui_weak.clone());
                btn_done(&ui_weak, AppWindow::set_pin_orbits_threads_working, AppWindow::set_pin_orbits_threads_done);
            });
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let vocals_in = vocals_in.clone();
        ui.on_boot_vocals(move || {
            btn_press(&ui_weak, AppWindow::set_boot_vocals_working, AppWindow::set_boot_vocals_done);
            tokio::spawn(boot_server(
                "vocals",
                VOCALS_PORT,
                vocals_args(),
                None,
                vocals_in.clone(),
                None,
                Some(NodeId::Vocals),
                ui_weak.clone(),
            ));
            btn_done(&ui_weak, AppWindow::set_boot_vocals_working, AppWindow::set_boot_vocals_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_boot_tidal(move || {
            btn_press(&ui_weak, AppWindow::set_boot_tidal_working, AppWindow::set_boot_tidal_done);
            tokio::spawn(spawn_proc(
                "tidal",
                tidal_args(),
                None,
                tidal_in.clone(),
                Some(("Connected to SuperDirt.", NodeId::Tidal)),
                Some(NodeId::Tidal),
                ui_weak.clone(),
            ));
            btn_done(&ui_weak, AppWindow::set_boot_tidal_working, AppWindow::set_boot_tidal_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_tidal_test_pattern(move || {
            btn_press(&ui_weak, AppWindow::set_tidal_test_pattern_working, AppWindow::set_tidal_test_pattern_done);
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                r#"b2 $ s "bd(3, 4)" "#.to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_tidal_test_pattern_working, AppWindow::set_tidal_test_pattern_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_tidal_hush(move || {
            btn_press(&ui_weak, AppWindow::set_tidal_hush_working, AppWindow::set_tidal_hush_done);
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                "hush".to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_tidal_hush_working, AppWindow::set_tidal_hush_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let tidal_in = tidal_in.clone();
        ui.on_stop_tidal(move || {
            btn_press(&ui_weak, AppWindow::set_stop_tidal_working, AppWindow::set_stop_tidal_done);
            // :quit lets ghci shut down cleanly; the wait task clears the node.
            tokio::spawn(send_line(
                "tidal",
                tidal_in.clone(),
                ui_weak.clone(),
                ":quit".to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_stop_tidal_working, AppWindow::set_stop_tidal_done);
        });
    }

    // ── sidebar callbacks ────────────────────────────────────────
    {
        let ui_weak = ui_weak.clone();
        ui.on_quantum_apply(move || {
            btn_press(&ui_weak, AppWindow::set_quantum_apply_working, AppWindow::set_quantum_apply_done);
            let idx = ui_weak
                .upgrade()
                .map(|ui| ui.get_quantum_index())
                .unwrap_or(4);
            let size = QUANTUM_SIZES
                .get(idx as usize)
                .copied()
                .unwrap_or(512);
            log_line(&ui_weak, format!("$ pw-metadata -n settings 0 clock.force-quantum {size}"));
            let ui_weak = ui_weak.clone();
            apply_quantum(&ui_weak, size);
            // apply_quantum spawns its own blocking task and logs on
            // completion; mark the button done once the call returns.
            btn_done(&ui_weak, AppWindow::set_quantum_apply_working, AppWindow::set_quantum_apply_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let ctl = relay_ctl.clone();
        let tidal_in = tidal_in.clone();
        ui.on_relay_start(move || {
            btn_press(&ui_weak, AppWindow::set_relay_start_working, AppWindow::set_relay_start_done);
            let ctl = ctl.clone();
            let tidal_in = tidal_in.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                relay_start(&ctl, &tidal_in, &ui_weak).await;
                btn_done(&ui_weak, AppWindow::set_relay_start_working, AppWindow::set_relay_start_done);
            });
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let ctl = relay_ctl.clone();
        ui.on_relay_stop(move || {
            btn_press(&ui_weak, AppWindow::set_relay_stop_working, AppWindow::set_relay_stop_done);
            let ctl = ctl.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                relay_stop(&ctl, &ui_weak).await;
                btn_done(&ui_weak, AppWindow::set_relay_stop_working, AppWindow::set_relay_stop_done);
            });
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let ctl = relay_ctl.clone();
        let tidal_in = tidal_in.clone();
        ui.on_relay_restart(move || {
            btn_press(&ui_weak, AppWindow::set_relay_restart_working, AppWindow::set_relay_restart_done);
            let ctl = ctl.clone();
            let tidal_in = tidal_in.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                relay_restart(&ctl, &tidal_in, &ui_weak).await;
                btn_done(&ui_weak, AppWindow::set_relay_restart_working, AppWindow::set_relay_restart_done);
            });
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_midi_refresh(move || {
            btn_press(&ui_weak, AppWindow::set_midi_refresh_working, AppWindow::set_midi_refresh_done);
            let uw = ui_weak.clone();
            tokio::task::spawn_blocking(move || {
                // ModelRc is Rc-based and not Send, so only the raw rows
                // cross the thread; the model is built on the UI thread.
                let rows: Vec<MidiRow> = midi::list_devices()
                    .into_iter()
                    .map(|d| MidiRow {
                        id: d.id.into(),
                        name: d.name.into(),
                        direction: d.direction.into(),
                    })
                    .collect();
                let uw = uw.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_midi_rows(ModelRc::new(VecModel::from_slice(&rows)));
                    }
                    btn_done(&uw, AppWindow::set_midi_refresh_working, AppWindow::set_midi_refresh_done);
                });
            });
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_midi_reconnect(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let rows = ui.get_midi_rows();
            let Some(row) = (i >= 0 && (i as usize) < rows.row_count())
                .then(|| rows.row_data(i as usize))
                .flatten()
            else {
                return;
            };
            // Stub: the real reconnect sends the device setup code to sclang
            // (MIDIClient.init + MIDIOut.newByName + ~dirt.soundLibrary.addMIDI).
            log_line(
                &ui_weak,
                format!("[midi] reconnect {} ({}): not wired yet", row.name, row.id),
            );
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_orbit_mute(move |g, i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let groups = ui.get_mute_groups();
            let Some(group) = (g >= 0 && (g as usize) < groups.row_count())
                .then(|| groups.row_data(g as usize))
                .flatten()
            else {
                return;
            };
            let items = group.items;
            let Some(mut item) = (i >= 0 && (i as usize) < items.row_count())
                .then(|| items.row_data(i as usize))
                .flatten()
            else {
                return;
            };
            item.muted = !item.muted;
            if let Some(vm) = items.as_any().downcast_ref::<VecModel<MuteItemRow>>() {
                vm.set_row_data(i as usize, item.clone());
            }
            // Stub: wiring lands with Tidal's /mute control or an sclang-side
            // orbit mute; the button state is authoritative in the meantime.
            log_line(
                &ui_weak,
                format!("[mutes] {} muted={} (not wired yet)", item.label, item.muted),
            );
        });
    }

    // Recording. The label lands in both an sclang string literal and a
    // filesystem path, so keep it to characters that are safe in both.
    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_rec_start(move || {
            btn_press(&ui_weak, AppWindow::set_rec_start_working, AppWindow::set_rec_start_done);
            let label = ui_weak
                .upgrade()
                .map(|ui| ui.get_session_name().to_string())
                .unwrap_or_default();
            let label: String = label
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let label = if label.is_empty() {
                String::from(labels::REC_FALLBACK_SESSION)
            } else {
                label
            };
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                format!("~hubRecStart.value(\"{label}\");"),
            ));
            btn_done(&ui_weak, AppWindow::set_rec_start_working, AppWindow::set_rec_start_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let orbits_in = orbits_in.clone();
        ui.on_rec_stop(move || {
            btn_press(&ui_weak, AppWindow::set_rec_stop_working, AppWindow::set_rec_stop_done);
            tokio::spawn(send_line(
                "orbits",
                orbits_in.clone(),
                ui_weak.clone(),
                "~hubRecStop.value;".to_string(),
            ));
            btn_done(&ui_weak, AppWindow::set_rec_stop_working, AppWindow::set_rec_stop_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let paths = session_paths.clone();
        ui.on_rec_refresh(move || {
            btn_press(&ui_weak, AppWindow::set_rec_refresh_working, AppWindow::set_rec_refresh_done);
            let uw = ui_weak.clone();
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || {
                let sessions = recordings::list_sessions();
                *paths.lock().unwrap() = sessions.iter().map(|s| s.path.clone()).collect();
                let rows: Vec<SessionRow> = sessions
                    .into_iter()
                    .map(|s| SessionRow {
                        name: s.name.into(),
                        info: format!("{} stems", s.stems).into(),
                    })
                    .collect();
                let uw = uw.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_sessions(ModelRc::new(VecModel::from_slice(&rows)));
                        ui.set_stems(ModelRc::new(VecModel::from_slice(
                            &Vec::<StemRow>::new(),
                        )));
                        ui.set_selected_session(-1);
                    }
                    btn_done(&uw, AppWindow::set_rec_refresh_working, AppWindow::set_rec_refresh_done);
                });
            });
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let paths = session_paths.clone();
        ui.on_select_session(move |i| {
            let dir = { paths.lock().unwrap().get(i as usize).cloned() };
            let Some(dir) = dir else { return };
            let uw = ui_weak.clone();
            tokio::task::spawn_blocking(move || {
                let rows: Vec<StemRow> = recordings::list_stems(&dir)
                    .into_iter()
                    .map(|s| StemRow {
                        name: s.name.into(),
                        size: s.size.into(),
                        duration: s.duration.into(),
                    })
                    .collect();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_stems(ModelRc::new(VecModel::from_slice(&rows)));
                        ui.set_selected_session(i);
                    }
                });
            });
        });
    }

    {
        let ui_weak = ui_weak.clone();
        let paths = session_paths.clone();
        ui.on_rec_open_folder(move || {
            btn_press(&ui_weak, AppWindow::set_rec_open_folder_working, AppWindow::set_rec_open_folder_done);
            let selected = ui_weak
                .upgrade()
                .map(|ui| ui.get_selected_session())
                .unwrap_or(-1);
            let dir = if selected >= 0 {
                paths.lock().unwrap().get(selected as usize).cloned()
            } else {
                None
            }
            .unwrap_or_else(|| PathBuf::from(recordings::RECORDINGS_DIR));

            if !dir.is_dir() {
                log_line(
                    &ui_weak,
                    format!("[rec] nothing to open yet: {}", dir.display()),
                );
                btn_done(&ui_weak, AppWindow::set_rec_open_folder_working, AppWindow::set_rec_open_folder_done);
                return;
            }
            log_line(&ui_weak, format!("[rec] opening {}", dir.display()));
            let _ = Command::new("xdg-open").arg(&dir).spawn();
            btn_done(&ui_weak, AppWindow::set_rec_open_folder_working, AppWindow::set_rec_open_folder_done);
        });
    }

    // ── bench stub ───────────────────────────────────────────────
    {
        let ui_weak = ui_weak.clone();
        ui.on_bench_run(move || {
            btn_press(&ui_weak, AppWindow::set_bench_run_working, AppWindow::set_bench_run_done);
            log_line(
                &ui_weak,
                "[bench] song runner embed not wired yet -- use tools/songrun.py".into(),
            );
            btn_done(&ui_weak, AppWindow::set_bench_run_working, AppWindow::set_bench_run_done);
        });
    }

    // ── danger row ───────────────────────────────────────────────
    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_orbits(move || {
            btn_press(&ui_weak, AppWindow::set_panic_orbits_working, AppWindow::set_panic_orbits_done);
            log_line(
                &ui_weak,
                format!("[orbits] panic triggered: ending port {ORBITS_PORT}"),
            );
            kill_orbits(&ui_weak);
            btn_done(&ui_weak, AppWindow::set_panic_orbits_working, AppWindow::set_panic_orbits_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_panic_vocals(move || {
            btn_press(&ui_weak, AppWindow::set_panic_vocals_working, AppWindow::set_panic_vocals_done);
            log_line(
                &ui_weak,
                format!("[vocals] panic triggered: ending port {VOCALS_PORT}"),
            );
            kill_server(&ui_weak, VOCALS_PORT);
            btn_done(&ui_weak, AppWindow::set_panic_vocals_working, AppWindow::set_panic_vocals_done);
        });
    }

    {
        let ui_weak = ui_weak.clone();
        ui.on_force_restart_all(move || {
            btn_press(&ui_weak, AppWindow::set_force_restart_all_working, AppWindow::set_force_restart_all_done);
            log_line(
                &ui_weak,
                "[system] force restart: clearing all scsynth/sclang instances".into(),
            );
            log_line(&ui_weak, "$ pkill -f scsynth".into());
            let _ = Command::new("pkill").arg("-f").arg("scsynth").status();
            log_line(&ui_weak, "$ pkill -f sclang".into());
            let _ = Command::new("pkill").arg("-f").arg("sclang").status();
            // A live Tidal keeps writing to the dead stack's busses.
            set_node_state(&ui_weak, NodeId::Tidal, NodeState::Off);
            log_line(
                &ui_weak,
                "[chain] restart Tidal after the stack comes back".into(),
            );
            btn_done(&ui_weak, AppWindow::set_force_restart_all_working, AppWindow::set_force_restart_all_done);
        });
    }

    ui.run()
}
