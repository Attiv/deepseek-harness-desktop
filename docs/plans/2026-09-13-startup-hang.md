# Startup Hang Implementation Plan

**Goal:** Restore desktop startup progression without changing DSH or restarting the current backend.

**Architecture:** Preserve synchronous HTTP/auth/startup code and move its loop out of an async task into a blocking worker. Keep the existing 600-second deadline and owned-process cleanup.

**Tech Stack:** Rust, Tauri 2, reqwest blocking, Python unittest.

## Task 1: Confirm and reproduce
- Symbolicate the saved sample using a diagnostic release build.
- Add a test around the production startup dispatcher: send 160 loopback requests, bounded by recv_timeout, and check completion.
- Add source guard in tests/test_launcher_command.py proving the actual boot loop uses that dispatcher.
- Run targeted tests on old dispatch and require failure (debug panic or release stall).

## Task 2: Minimal fix
- In src-tauri/src/main.rs, introduce a synchronous startup worker dispatch entry and use tauri::async_runtime::spawn_blocking(move || ...), not spawn(async move ...).
- Do not alter download command, credentials, request timeouts or shutdown ownership.
- Re-run regression and full suites: python3 -m unittest discover -s tests -v; cargo test --bin dsh-app.

## Task 3: Verify and deliver
- Build release app with npm exec -- tauri build --bundles app.
- Inspect git diff for accidental changes; apply only fix delta back to original dirty workspace.
- Leave running backend intact. Report artifact path and explicitly distinguish automated startup validation from manual WebView launch validation.
