# Owned Backend Process Tree Shutdown Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ensure every real application-exit path terminates the complete `pnpm → node → dsh` process tree started by this desktop app without touching unrelated processes.

**Architecture:** Launch the backend shell in an app-owned process group on Unix and retain the existing owned root PID on Windows. Route menu and Tauri lifecycle exits through one idempotent cleanup function that atomically takes `DshChild`, gracefully terminates the Unix group, force-kills survivors, or invokes Windows `taskkill /T /F`.

**Tech Stack:** Rust 2021, Tauri v2 lifecycle events, Unix process groups/signals via `libc`, Windows process creation flags and `taskkill`, Python `unittest` source regressions.

---

### Task 1: Specify process-tree termination behavior

**Files:**
- Modify: `src-tauri/src/main.rs:903-976`
- Modify: `tests/test_launcher_command.py`

**Step 1: Write the failing Rust test**

Add a Unix-only test that starts a shell in its own process group, makes both the shell and a background child ignore `SIGTERM`, calls the desired `terminate_child_tree(&mut child)` API, then polls `libc::kill(-pgid, 0)` and asserts the group no longer exists. Ignoring `SIGTERM` proves the force-kill fallback handles descendants rather than only the immediate shell.

```rust
#[cfg(unix)]
#[test]
fn terminates_entire_unix_process_group() {
    use std::os::unix::process::CommandExt;

    let mut child = Command::new("sh")
        .args(["-c", "trap '' TERM; sleep 30 & wait"])
        .process_group(0)
        .spawn()
        .expect("spawn test process group");
    let pgid = child.id() as libc::pid_t;

    terminate_child_tree(&mut child);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && unsafe { libc::kill(-pgid, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_ne!(unsafe { libc::kill(-pgid, 0) }, 0);
}
```

**Step 2: Write failing exit-path source regressions**

Extend `LauncherCommandTests` to require:

```python
def test_menu_quit_stops_the_owned_backend(self):
    source = self.source
    self.assertIn('"quit" => {\n                    stop_owned_dsh(app);', source)

def test_final_tauri_exit_stops_the_owned_backend(self):
    source = self.source
    self.assertIn("tauri::RunEvent::Exit", source)
    self.assertNotIn("tauri::RunEvent::ExitRequested", source)

def test_unix_backend_uses_an_owned_process_group(self):
    self.assertIn("cmd.process_group(0);", self.source)
```

Refactor `setUp()` to read `main.rs` once into `self.source`.

**Step 3: Run tests to verify RED**

Run:

```bash
python3 -m unittest tests/test_launcher_command.py -v
cd src-tauri && cargo test terminates_entire_unix_process_group -- --nocapture
```

Expected: Python assertions fail because menu/final-exit cleanup hooks and process groups are absent; Rust compilation fails because `terminate_child_tree` and direct `libc` usage are absent.

**Step 4: Commit the failing tests**

```bash
git add tests/test_launcher_command.py src-tauri/src/main.rs
git commit -m "test: cover owned backend process cleanup"
```

### Task 2: Create and terminate the owned process tree

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Modify: `src-tauri/src/main.rs:11-14,254-352`

**Step 1: Add the direct Unix signal dependency**

Add:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"
```

**Step 2: Put Unix children into a dedicated process group**

Import Unix `CommandExt` under `#[cfg(unix)]`, then configure every non-Windows backend command after construction:

```rust
#[cfg(unix)]
{
    cmd.process_group(0);
}
```

On Windows combine the existing no-window flag with a new-process-group flag:

```rust
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
```

**Step 3: Implement platform-specific tree termination**

Add `terminate_child_tree(child: &mut Child)`. On Unix:

1. Derive the process group ID from `child.id()`.
2. Send `SIGTERM` to `-pgid`.
3. Sleep for a short bounded grace period.
4. Send `SIGKILL` to `-pgid` regardless of whether the group leader already exited, so stubborn descendants are covered.
5. Call `child.wait()` to reap the direct child.

On Windows invoke `taskkill /PID <pid> /T /F`, retain `CREATE_NO_WINDOW`, and call `child.wait()` afterward. Keep cleanup best-effort and non-panicking.

**Step 4: Run the focused test to verify GREEN**

Run:

```bash
cd src-tauri && cargo test terminates_entire_unix_process_group -- --nocapture
```

Expected: PASS and completion in well under two seconds.

**Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/main.rs
git commit -m "fix: terminate the owned backend process tree"
```

### Task 3: Route every exit path through idempotent cleanup

**Files:**
- Modify: `src-tauri/src/main.rs:694-900`

**Step 1: Implement the state-level cleanup function**

Add:

```rust
fn stop_owned_dsh(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<DshChild>() else {
        return;
    };
    let Ok(mut owned_child) = state.0.lock() else {
        return;
    };
    if let Some(mut child) = owned_child.take() {
        terminate_child_tree(&mut child);
    }
}
```

The `take()` makes cleanup idempotent across the menu handler and final `Exit` event.

**Step 2: Replace inline menu cleanup**

Make the `"quit"` handler call `stop_owned_dsh(app)` immediately before `app.exit(0)`. Delete the duplicated platform-specific inline kill code.

**Step 3: Add Tauri lifecycle cleanup**

Update the final `run` callback:

```rust
.run(|app, event| {
    if matches!(event, tauri::RunEvent::Exit) {
        stop_owned_dsh(app);
    }
});
```

Do not clean up on the cancellable `RunEvent::ExitRequested`: the request may be cancelled, in which case the application and backend must both keep running. Do not change `CloseRequested`: closing the main window must continue hiding it and preserving the backend.

**Step 4: Run source regressions to verify GREEN**

Run:

```bash
python3 -m unittest tests/test_launcher_command.py -v
```

Expected: all launcher regression tests PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/main.rs tests/test_launcher_command.py
git commit -m "fix: clean up backend on every app exit path"
```

### Task 4: Document and verify the finished behavior

**Files:**
- Modify: `README.md`

**Step 1: Document ownership-scoped shutdown**

Add a feature/workflow note that real application exit terminates only the `pnpm/node/dsh` tree created by the desktop app, while closing the main window merely hides it and reused external services remain untouched.

**Step 2: Format and run the complete verification suite**

Run:

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
python3 -m unittest discover -s tests -v
cargo test --manifest-path src-tauri/Cargo.toml
cargo check --release --manifest-path src-tauri/Cargo.toml
git diff --check
```

Expected: every command exits with status 0; Rust reports all non-ignored tests passing; Python reports all tests passing; `git diff --check` prints nothing.

**Step 3: Inspect the final diff and ownership guarantees**

Run:

```bash
git diff --stat HEAD~3
git diff HEAD~3 -- src-tauri/src/main.rs src-tauri/Cargo.toml tests/test_launcher_command.py README.md
```

Confirm that no process-name-wide kill (`pkill`, `killall`, or equivalent) was introduced and the externally reused backend remains represented by `DshChild(None)`.

**Step 4: Commit documentation**

```bash
git add README.md
git commit -m "docs: explain backend cleanup on exit"
```
