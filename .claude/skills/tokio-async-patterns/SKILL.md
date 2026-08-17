---
name: tokio-async-patterns
description: Use this skill whenever writing or editing Rust code that uses tokio::spawn, async move blocks or closures, shared state across async tasks (Arc<Mutex<...>>, channels, Weak handles), or code that reads output from a long-running child process (tokio::process::Command with piped stdout/stderr, BufReader, AsyncBufReadExt::lines()). Also use it whenever `let _ = ...` appears in Rust and you need to judge whether discarding that Result is intentional, or whenever streamed output (child-process logs, sensor readings, event streams) needs to be aggregated into a shared buffer that another task or a UI reads. Trigger this even if the request doesn't say "tokio" or "async" explicitly, as long as the code has async fn, .await, or tokio:: in it.
---

## Why this skill exists

Tokio's ownership rules are stricter than sync Rust in one specific way: everything moved into `async move` must be `'static` — own its data rather than borrow it — because a spawned task can outlive the function that spawned it. That single constraint produces a small, very repeatable set of mistakes around cloning shared handles, plus a few conventions (like `let _ = ...`) that look like errors but are deliberate. This skill spells them out explicitly.

## Stop-and-ask rule — read this first

Count your tool calls as you work through an async ownership issue. If you reach **more than 3 tool calls** (reads/edits/bash) OR **more than 3 failed compile/test attempts**, stop immediately. Do not keep guessing. Show the user the exact compiler error, explain what you already tried, and ask how they'd like to proceed. Sprinkling `.clone()` or `Arc::new()` around at random until something compiles is not a valid strategy — ask instead.

## 1. `move` in spawned tasks: what it actually means

```rust
tokio::spawn(async move {
    // body
});
```

`async move` forces the block to take ownership of every external variable it references, instead of borrowing them. This is required because the task may run long after the function that created it has returned — it cannot hold a borrow into that function's stack frame.

**Consequence**: any variable referenced inside `async move` is consumed at the point the block is *created*, not merely when it runs. If you need that variable again afterward, or need to spawn several tasks that each need their own copy, clone it *before* the block — not inside it, not after.

## 2. Clone before you move — every time, especially across loops and repeatable closures

The types typically shared across tasks are `Arc<T>`, `Arc<Mutex<T>>`, or `slint::Weak<T>` (see the `slint-rust-integration` skill for the Slint-specific case). None of these are `Copy`. All are cheap to `.clone()` — it's a refcount bump, not a deep copy — so clone liberally rather than fighting the borrow checker to avoid it.

**Broken** — reusing one handle across two callback registrations:
```rust
let log = Arc::new(Mutex::new(Vec::new()));

ui.on_boot_orbits(move || {
    tokio::spawn(handle_orbits(log)); // moves `log`
});

ui.on_boot_vocals(move || {
    tokio::spawn(handle_vocals(log)); // ERROR: `log` already moved above
});
```

**Fixed** — clone right before each place that needs its own owned copy:
```rust
let log = Arc::new(Mutex::new(Vec::new()));

{
    let log = log.clone();
    ui.on_boot_orbits(move || {
        tokio::spawn(handle_orbits(log.clone())); // cloned again: this closure can fire more than once
    });
}
{
    let log = log.clone();
    ui.on_boot_vocals(move || {
        tokio::spawn(handle_vocals(log.clone()));
    });
}
```

Notice the **double clone** in the fixed version: one clone to move an owned copy into the `on_boot_orbits` closure, and a second clone *inside* that closure body, because `tokio::spawn` needs a fresh owned copy every time the closure runs — and a `Fn` callback like `on_x` can run repeatedly (e.g. once per button click). This looks redundant but is required.

**Rule of thumb**: if shared state crosses (a) a loop boundary, (b) a callback that can fire more than once, or (c) more than one `tokio::spawn`, it needs a `.clone()` immediately before each of those boundaries — not once at the top of the function.

## 3. The `let _ = ...` pattern: intentional, not sloppy

You'll see this constantly:
```rust
let _ = slint::invoke_from_event_loop(move || { ... });
let _ = Command::new("pkill").arg("-f").arg("scsynth").status();
tokio::spawn(async move { let _ = child.wait().await; });
```

`Result`-returning calls are marked `#[must_use]`, so Rust must either use the result or explicitly discard it. `let _ = expr;` says "I am deliberately not checking whether this succeeded, and that's a considered decision" — it silences the warning on purpose.

**Appropriate**: fire-and-forget operations where failure is unrecoverable, not actionable, or already visible some other way — killing a process that might already be dead (`pkill` returning non-zero just means "nothing to kill"), scheduling a UI update on a window that may have already closed, or reaping a child process whose exit code nothing downstream cares about.

**Not appropriate**: silencing a warning on a call whose failure genuinely matters — e.g. the `Command::spawn()` that boots an application's core process. If that fails, the caller needs to know; match on the `Result` and surface or log the error instead of discarding it.

If you're unsure which case you're in, don't default to `let _ =` — ask the user whether the failure case matters here.

## 4. Streaming output from a long-running child process

The common task: boot a child process (an audio server, a build tool, any long-lived subprocess) and continuously read its stdout/stderr into shared state that a UI or another part of the app can display, without blocking anything else.

```rust
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

let mut child = tokio::process::Command::new("some-long-running-binary")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// One task per stream — stdout and stderr must be drained independently.
// If you only read one, a full pipe buffer on the other can stall the child process.
if let Some(stdout) = child.stdout.take() {
    let shared_state = shared_state.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            push_into_shared_state(&shared_state, line);
        }
    });
}

if let Some(stderr) = child.stderr.take() {
    let shared_state = shared_state.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            push_into_shared_state(&shared_state, line);
        }
    });
}

// Reap the child so it doesn't become a zombie. Nothing else here needs
// its exit status, so discarding the result is the appropriate use of `let _ =`.
tokio::spawn(async move {
    let _ = child.wait().await;
});
```

Key points:
- Use **`tokio::process::Command`**, not `std::process::Command`, whenever you need to `.await` on reading output asynchronously. `std::process::Command::spawn()` is still fine for fire-and-forget processes you never read from, but its `Child` type has no async I/O.
- **Take each stream once** (`.stdout.take()` / `.stderr.take()`) — this moves the handle out of `Child`, which is what lets you move it into a separately spawned task.
- **One spawned task per stream**, never one task trying to service both — `BufReader::lines()` awaits a line at a time, so a single task can't drain two streams concurrently.
- **Always spawn something that `.wait()`s on the child**, even if the exit code is unused, or the process can linger as a zombie.

### Aggregating into a shared buffer

A size-capped `Arc<Mutex<Vec<String>>>` is the standard shape for "last N lines of output visible to a UI":

```rust
type LogBuf = Arc<Mutex<Vec<String>>>;
const MAX_LOG_LINES: usize = 200;

fn push_into_shared_state(log: &LogBuf, line: String) {
    let mut lines = log.lock().unwrap();
    lines.push(line);
    if lines.len() > MAX_LOG_LINES {
        let excess = lines.len() - MAX_LOG_LINES;
        lines.drain(0..excess);
    }
}
```

Cap the buffer (`MAX_LOG_LINES`) — a long-running process produces unbounded output, and an ever-growing `Vec<String>` is a slow-motion memory leak. Drain from the front so only the most recent lines survive.

Use `std::sync::Mutex`, not `tokio::sync::Mutex`, as long as the lock is never held across an `.await` point — the pattern above only holds it for a synchronous `push`/`drain`, so the std lock is simpler and faster. If you ever need to `.await` while holding the lock, switch to `tokio::sync::Mutex` — holding a std `Mutex` across an await point risks deadlocking the runtime.

## 5. Quick diagnostic checklist

1. "Use of moved value" on a shared handle? Clone it immediately before the point it gets moved — check every loop iteration and every closure that can run more than once (Section 2).
2. Lifetime or "cannot borrow" error inside `tokio::spawn`? Something borrowed is crossing into the `async move` block — clone it instead of trying to borrow across the spawn boundary (Section 1).
3. About to write `let _ = ...`? Confirm the failure case genuinely doesn't matter before discarding the `Result` (Section 3).
4. Streaming a subprocess's output? Use `tokio::process::Command`, pipe both stdout and stderr, and give each stream its own task (Section 4).
5. Shared output buffer growing without bound? Cap it and drain old entries (Section 4).

If you've worked through this list and you're still stuck after 3 attempts, stop and ask the user — see the stop-and-ask rule above.
