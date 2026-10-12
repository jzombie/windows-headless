# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [Unreleased]

### Changed

- SSH stack `russh` 0.52.1 → 0.64.1 (plus transitive updates).
- PNG decoder `png` 0.17.16 → 0.18.1.
- ISO helpers `hadris-fat` / `hadris-udf` 2.4.0 → 2.5.0.
- Runtime `tokio` 1.53.1 → 1.53.2.
- All via Dependabot; no behavior changes expected.

## [0.1.0-alpha] - 10-11-2026

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
- Watchable installs: follow along live over VNC with screenshots saved
  as it goes; if a boot dies, the log tells you what was on screen and
  why, instead of leaving you guessing.
- Proven-install log: each green install records itself in
  `tested-isos.jsonl` and refreshes the README's Tested table.
- Fast reboots: the first proven boot state is snapshotted, and later
  boots rewind straight to it.
- Rust library surface: `VmConfig`, `HeadlessVmBuilder`, `DiskManager`,
  and friends are exported for programmatic use.

### Prerequisites

- A recent Rust toolchain (1.85+); no prebuilt binaries yet.
- Guest arch must match host arch (ARM Windows on Apple Silicon Macs).
