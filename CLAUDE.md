# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- `cargo test` — unit tests live inline in `#[cfg(test)] mod tests` blocks. Tests under `src/platform/linux/` only compile and run on Linux.
- `cargo check` — native type-check. Shared code must also compile for the other OS: CI (`.github/workflows/release.yml`) runs `cargo build`/`cargo test` with `--locked` on ubuntu-24.04 and windows-latest. From Linux, `cargo check --target x86_64-pc-windows-msvc` checks the Windows side; from Windows there is no Linux cross-check setup, so rely on CI.
- Don't run `cargo fmt` across the repo: there is no rustfmt config, CI doesn't check formatting, and the tree is not currently rustfmt-clean, so it rewrites unrelated code.
- DDC behavior needs a real monitor and can't be unit-tested. Verify with tests and type-checks; don't launch the tray app during a session.

## Architecture

- One crate: shared logic (config, daemon, egui Settings window, CLI) in `src/`, with every OS difference behind `src/platform/`. `src/platform/mod.rs` documents the exact contract each OS module must provide (`Ui`, `ddc`, `ipc`, autostart, …).
- The tray app (`daemon.rs`) owns state on one event loop thread; DDC runs on a dedicated worker thread so operations never overlap. The Settings window is a separate child process (`monitor-input-switcher settings-window`) so the tray process stays small — it is not a second window in the daemon process.
- CLI commands are sent to the running app over a Unix socket / named pipe and fall back to talking to the monitor directly when it isn't running.
- No logging framework: diagnostics are `eprintln!`. On Windows the binary is built with `windows_subsystem = "windows"`, so `platform::attach_console()` re-attaches CLI invocations to the parent console.

## Gotchas

- Config JSON keys are PascalCase (`#[serde(rename_all = "PascalCase")]`, documented in the README). Every new field needs a serde default so existing `config.json` files still load after an upgrade.
- Reading Input Source (VCP `0x60`) often returns junk in the high byte (`0x1B0F`, `0x0F0F`); only the low byte is the input. Compare with `ddc::matches`, never plain equality.
- Input codes, names and capabilities are per monitor — don't hard-code a model or assume the full MCCS table.
- Cross-checking the Windows build from Linux without MSVC's `rc.exe` prints "Could not embed the Windows icon"; that's expected, not a failure.
- Release: bump `version` in `Cargo.toml`, then push tag `v<version>` — CI fails if the tag and manifest version differ.

## Repo etiquette

- Do not commit. Leave changes in the working tree for the user to review and commit.
