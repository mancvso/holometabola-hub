---
name: slint-rust-integration
description: Use this skill whenever writing, editing, or debugging Rust code that integrates with a Slint UI — files ending in .slint, slint::include_modules!(), AppWindow or other ComponentHandle types, callbacks wired with on_name, or properties read/written with get_name/set_name. Also use it whenever a Weak reference from Slint (slint::Weak, .as_weak(), .upgrade()) appears anywhere in the code, or whenever a closure captures a Slint UI handle inside a loop, a tokio::spawn, or an event-loop callback — these are the most common source of Rust borrow-checker errors and silent UI-update bugs in Slint apps. Trigger this skill even if the user's request doesn't mention "Slint" by name, as long as .slint files or Slint types are visible in the codebase.
---

## Why this skill exists

Slint is a declarative UI toolkit: the visual structure lives in `.slint` files, the behavior lives in Rust, and a build step glues them together. This split, plus Slint's use of `Weak` handles so background tasks don't keep a closed window alive forever, produces a small, very repeatable set of mistakes. This skill lists them explicitly so you don't have to rediscover them by trial and error.

## Stop-and-ask rule — read this first

Count your tool calls as you work through a Slint issue. If you reach **more than 3 tool calls** (reads/edits/bash) OR **more than 3 failed compile/test attempts** while applying this skill, stop immediately. Do not keep guessing. Show the user the exact compiler error you're stuck on, explain what you already tried, and ask how they'd like to proceed. Silently retrying variations of the same fix is more expensive than asking, and it risks leaving the code in a worse state than when you started.

## 1. Weak references: the single most common bug

Slint UI handles (e.g. `AppWindow`) are not `Clone` and cannot be moved directly into a background task. To reference the UI from a `tokio::spawn`, a thread, or a timer, get a `Weak` handle instead:

```rust
let ui = AppWindow::new()?;
let ui_weak = ui.as_weak(); // cheap; does NOT keep the window alive
```

### `Weak` is not `Copy` — clone it before every move, not just once

`ui_weak` has type `slint::Weak<AppWindow>`. It implements `Clone` but not `Copy`. That means:

- Moving it into a closure **consumes** it.
- If that closure is constructed more than once — inside a `loop`, inside a function called repeatedly, inside more than one `.on_xxx()` registration — you must clone it fresh before each move, not just once at the top of the function.

**Broken** — compiles the first time you write it, then fails on the second loop iteration with `E0382: use of moved value`:
```rust
tokio::spawn(async move {
    loop {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() { /* ... */ }
        });
        // ui_weak was moved into the closure above.
        // Next iteration: the compiler has nothing left to move.
    }
});
```

**Fixed** — clone *inside* the loop, immediately before the point it gets moved:
```rust
tokio::spawn(async move {
    loop {
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() { /* ... */ }
        });
    }
});
```

The same rule applies when wiring up more than one callback that each need the UI handle — clone once per callback, and wrap each registration in its own block so the clone stays visually scoped to the closure that consumes it:
```rust
{
    let ui_weak = ui_weak.clone();
    ui.on_boot_orbits(move || { /* uses ui_weak */ });
}
{
    let ui_weak = ui_weak.clone();
    ui.on_boot_vocals(move || { /* uses ui_weak */ });
}
```

### Always `.upgrade()`, and always handle `None`

`Weak::upgrade()` returns `Option<AppWindow>`. It is `None` once the window has been closed or dropped. Never `.unwrap()` this — a background task that outlives the window will panic. Always guard it:

```rust
if let Some(ui) = ui_weak.upgrade() {
    // safe to touch ui here
}
```

### UI mutations must happen on the UI/event-loop thread

You cannot call `ui.set_something(...)` directly from a spawned task or another thread — Slint's UI state isn't safe to mutate from arbitrary threads. Route every update through `slint::invoke_from_event_loop`:

```rust
let ui_weak = ui_weak.clone();
let _ = slint::invoke_from_event_loop(move || {
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_status("ONLINE".into());
    }
});
```
The `let _ =` here is deliberate, not sloppy — see the `tokio-async-patterns` skill for when discarding a `Result` like this is the right call.

## 2. Callbacks and properties: the naming conversion is mechanical, not guesswork

Slint auto-generates Rust bindings from the `.slint` file. Apply this table exactly — never guess a different casing:

| In `.slint` | Generated Rust |
|---|---|
| `callback my-thing();` | `ui.on_my_thing(\|args\| { ... });` and `ui.invoke_my_thing()` |
| `in property <string> my-value;` | `ui.set_my_value(...)` and `ui.get_my_value()` |
| `in-out property <int> counter;` | `ui.set_counter(...)` and `ui.get_counter()` |

Rules to apply:
- `.slint` identifiers use `kebab-case` (dashes). Generated Rust identifiers use `snake_case` (underscores) — `boot-orbits` becomes `on_boot_orbits` / `invoke_boot_orbits`.
- `out` properties are read-only from `.slint`'s own perspective (bound to an expression); Rust can still read them, but `.slint` markup shouldn't assign to them directly.
- If you add a `callback` or `property` to the `.slint` file, you must add the matching `.on_x(...)` or `.set_x(...)`/`.get_x(...)` call in the Rust wiring code — the two files never sync themselves.
- If a callback or property "doesn't exist" on the generated type according to the compiler, re-read the exact `.slint` declaration before assuming something deeper is broken — nine times out of ten it's a spelling or casing mismatch between the two files.

## 3. Code structure convention

- `.slint` files (typically under a `ui/` directory) declare visual structure, styling, and `callback`/`property` declarations only. Keep business logic out of them — simple expressions (`visible: a && b;`) are fine, spawning processes or doing I/O is not.
- Rust calls `slint::include_modules!();` once, near the top of the file that builds the window, to pull in the generated types. `build.rs` (via `slint_build::compile(...)`) determines which `.slint` file(s) get compiled — check there if a generated type seems to be missing entirely.
- All real behavior (spawning processes, file/network I/O, timers) lives in Rust, triggered by callbacks and reflected back to the UI via property setters.

## 4. Quick diagnostic checklist

When something won't compile or the UI won't update, check in this order:
1. Did you clone `Weak` immediately before each move into a closure that could run more than once? (Section 1)
2. Are you calling `.upgrade()` and handling `None`, rather than assuming the window is still alive?
3. Is the UI mutation happening inside `slint::invoke_from_event_loop`, not directly from a background task?
4. Does the Rust identifier (`on_x` / `set_x` / `get_x`) exactly match the kebab-case-to-snake_case conversion of the `.slint` declaration?
5. If you edited the `.slint` file, did you also update the matching Rust wiring, and vice versa?

If you've worked through this list and you're still stuck after 3 attempts, stop and ask the user — see the stop-and-ask rule above.
