---
name: launch-and-watch-debug-app
description: Use this skill whenever you need to visually verify a change to a compiled desktop app (Slint, GTK, egui, or any other native GUI Rust binary) by actually running the debug build, rather than just relying on `cargo build`/`cargo check` succeeding. Trigger it after editing UI code (.slint files, view/window construction, layout, callbacks) when the user asks to "test it", "check the UI", "see if it works", "launch the app", or when you're about to claim a UI change works — a successful compile only proves the code is well-typed, not that the feature behaves correctly on screen. Also use this skill any time you need to find or run the debug binary but don't already know its exact path — it derives the path from Cargo.toml instead of assuming one.
---

## Why this skill exists

A green `cargo build` only proves the code compiles — it says nothing about whether the window opens, the layout looks right, or the new button actually does what it's supposed to. The only way to actually verify a UI change is to run the compiled binary and watch what happens. But you (the model using this skill) most likely cannot see a native GUI window the way a human can — there is no screenshot of a Slint/GTK/egui window available to you. This skill's job is to get the app running on the user's real display and to give you the next best thing to eyes: the process's own stdout/stderr, and whether it stays alive.

**What this skill can confirm**: the app builds, launches, doesn't panic or crash on startup, and any log lines the code prints (status messages, error output, debug prints) look correct.
**What this skill cannot confirm**: that a button is in the right place, that text is legible, that colors look good, or any other purely visual judgment. If the user needs that kind of confirmation, tell them so explicitly and ask them to look at the running window themselves — don't claim visual success you can't actually observe.

## Step 1: Find the binary path — never hardcode it

Don't assume a binary name or path. Derive it from the project's own `Cargo.toml`, since the whole point is that this skill works in any Rust project, not just one specific repo.

```bash
find . -maxdepth 2 -name Cargo.toml -not -path "*/target/*"
```

Then read the `[package]` name field:
```bash
grep -m1 '^name' Cargo.toml | sed -E 's/name *= *"(.*)"/\1/'
```

The debug binary is normally at `target/debug/<that name>`. Confirm it actually exists before trying to run it:
```bash
ls -la "target/debug/<name>"
```

**Edge cases** — check for these before assuming the simple case above is correct:
- If `Cargo.toml` has a `[[bin]]` section with its own `name = "..."`, that name overrides the package name for the binary's filename — use the `[[bin]]` name instead.
- If `Cargo.toml` has a `[workspace]` section and no `[package]` section, this is a workspace root, not a single crate. Look for the actual binary crate among the workspace members (check each member's own `Cargo.toml`), or run `cargo metadata --no-deps --format-version 1` and locate the relevant package's `name` field in the JSON output.
- On Windows the binary has a `.exe` suffix; on Linux/macOS it does not. Check which platform you're on before assuming the bare name is correct — `uname -s` reports `Linux` on Linux and `Darwin` on macOS.

## Step 2: Build before you launch

```bash
cargo build
```

Read the output. If it fails, fix the compile error first — do not attempt to launch a binary from a failed or stale build. If you're re-testing after an edit, always rebuild; never assume the binary on disk reflects your latest change.

## Step 3: Confirm a display is available

How you check this depends on the OS, so detect it first:
```bash
uname -s
```

**Linux (`Linux`)** — a GUI binary needs an active X11 or Wayland session. Check:
```bash
echo "DISPLAY=$DISPLAY WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
```
If both are empty, there is no display to render into — launching will likely hang or fail immediately with a backend error. On Wayland specifically, also be aware that some GUI toolkits fall back to an XWayland/X11 backend unless explicitly told to use the native Wayland backend (toolkit-specific env vars, e.g. `SLINT_BACKEND=winit-wayland` for Slint) — if the window doesn't appear despite `WAYLAND_DISPLAY` being set, that's worth checking rather than concluding the display setup is broken.

**macOS (`Darwin`)** — there is no `DISPLAY`/`WAYLAND_DISPLAY` equivalent to check; native macOS apps render through the WindowServer of the current GUI login session automatically, with no env var gate. The thing that actually blocks a GUI launch on macOS is running **headless** — e.g. over a plain SSH session with no active GUI login, or inside a CI runner with no attached user session. If you're not sure whether a GUI session is available, the launch itself in Step 4 is the test: a `Symbol not found`/`NSApplication` failure or an immediate silent-exit-with-no-window in the captured log is the signal that there's no session to render into, not something you can rule out in advance with an env var check the way you can on Linux.

If you determine there's no display/session available on either platform, tell the user rather than retrying the launch — no amount of retrying fixes a missing display server.

## Step 4: Launch it and capture output, bounded by a timeout

Since you cannot watch the window directly, run the binary for a short, bounded window of time with its output redirected to a file, then inspect that file. Bound the run so it doesn't block forever if the app doesn't exit on its own (most GUI apps run an event loop and never exit until closed — that's expected and fine).

**Don't assume `timeout` exists** — it's a GNU coreutils command, standard on Linux but **not installed by default on macOS** (BSD userland). Check for it first, and fall back to a portable background-process pattern if it's missing:

```bash
if command -v timeout >/dev/null 2>&1; then
    timeout 6 "target/debug/<name>" > /tmp/app-run.log 2>&1
    echo "exit code: $?"
elif command -v gtimeout >/dev/null 2>&1; then
    # macOS with GNU coreutils installed via Homebrew (brew install coreutils) exposes it as gtimeout
    gtimeout 6 "target/debug/<name>" > /tmp/app-run.log 2>&1
    echo "exit code: $?"
else
    # Portable fallback for plain macOS/BSD with neither: background it, wait, then check/kill it yourself.
    "target/debug/<name>" > /tmp/app-run.log 2>&1 &
    APP_PID=$!
    sleep 6
    if kill -0 "$APP_PID" 2>/dev/null; then
        echo "exit code: 124 (still running after 6s, killing it)"
        kill "$APP_PID"
    else
        wait "$APP_PID"
        echo "exit code: $?"
    fi
fi
```

The three branches are written to report the same convention either way: **124 means "still alive after the timeout window,"** matching GNU `timeout`'s own exit code for that case, so the interpretation in the next section holds regardless of which branch actually ran.

Interpreting the exit code:
- **124** — `timeout` killed the process because it was still running after 6 seconds. For an event-loop-based GUI app, this is the *good* outcome: it means the app started, opened its window, and was still alive and responsive when the timer ended, rather than crashing.
- **0** — the app exited cleanly on its own before the timeout. Fine for a CLI-style tool; unexpected for a GUI app unless it's designed to exit immediately (e.g. a `--version` flag), so double check that's what you intended.
- **Any other non-zero code, or 101 (Rust panic)** — the app crashed. Read the captured log file — the panic message and backtrace (if `RUST_BACKTRACE=1` is set) will usually point straight at the bug.

Then read the captured output:
```bash
cat /tmp/app-run.log
```
An empty log with exit code 124 is a good sign — the app started and had nothing to complain about. A log full of error/warning lines, even with exit code 124, deserves a look — the app may be running in a degraded state.

If you need to watch output continuously while the app keeps running (e.g. to see log lines appear as you trigger something), run it in the background instead of using `timeout`, tail the log file, and remember to stop the process afterward rather than leaving it running indefinitely:
```bash
"target/debug/<name>" > /tmp/app-run.log 2>&1 &
APP_PID=$!
# ...watch /tmp/app-run.log, e.g. `tail -f` or repeated reads...
kill "$APP_PID"
```

## Step 5: Report what you actually verified — no more, no less

When you tell the user the result, be precise about what this process did and didn't check:
- "It builds and launches without crashing, and the log shows `[orbits] booting on port 57110...` as expected" is a claim you can back up.
- "The button is in the right place and looks good" is **not** something this skill lets you verify — say instead: "I can't see the rendered window from here; the app is running now if you want to check the layout yourself," and point them at the running instance if it's still up.

Never claim a visual/UX outcome (layout, color, alignment, "looks good") based only on logs or a clean exit code — that's exactly the gap this skill exists to be honest about.
