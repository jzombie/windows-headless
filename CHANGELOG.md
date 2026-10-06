# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [Unreleased]

First public shape (`0.1.0-alpha`): one command takes a Microsoft ISO and
hands back SSH into a headless Windows 11 VM. Tested on macOS (Apple
Silicon) and Linux (amd64 + arm64); Intel Macs are not supported.

### Added

- `build` / `run` flow: build the installer image from your ISO, boot it,
  and wait for SSH, or `run --win-iso` to do it all in one command.
- Docker-style sizing: `--memory 8g --cpus 8 --disk-size 128g`
  (`--smp` still works as an alias of `--cpus`).
- Automatic dependency provisioning: portable QEMU, wimlib, virtio
  drivers, and OpenSSH are fetched into a project cache on first run.
- Download integrity: every auto-downloaded byte is verified against
  hardcoded SHA-256 pins; `verify-pins` checks them all without installing.
- Install observability: phase banners, VNC URL + password, and a
  configurable screenshot time-lapse (`--screenshots-every-secs`, `0`
  disables) until SSH answers; dead boots end with a diagnosis block
  (screen state + last serial line), firmware reboot counting, and a
  `WARNING:` on confirmed modal failure dialogs (two screenshots in a row,
  once per episode).
- Proven-install log: each green install records itself in
  `tested-isos.jsonl` and refreshes the README's Tested table.
- Snapshots: `build`/`install` snapshot the proven shell state and `run`
  rewinds to it (dirty stops can't park the next boot at Recovery);
  `snapshot` re-takes it on demand (proves shell, halts cleanly,
  snapshots, stops), with an experimental `--live` migration-based
  variant (`snapshot --live` / `run --live`, see README tradeoffs).
- Rust library surface: `VmConfig`, `HeadlessVmBuilder`, `DiskManager`,
  and friends are exported for programmatic use.

### Prerequisites

- A recent Rust toolchain (1.85+); no prebuilt binaries yet.
- Guest arch must match host arch (ARM Windows on Apple Silicon Macs).
- On macOS, Gatekeeper needs a one-time quarantine clear (see README).
