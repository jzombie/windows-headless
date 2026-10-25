# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [UNRELEASED]

### Changed

- Runtime `tokio` 1.53.2 → 1.53.3 (plus transitive `windows-sys` 0.52.0 → 0.61.2).
- JSON `serde_json` 1.0.151 → 1.0.154.
- Hashing `sha2` 0.11.0 → 0.11.1.
- IPC `interprocess` 2.4.4 → 2.4.6.

## [0.2.0-alpha] - 10-12-2026

### Changed

- SSH stack `russh` 0.52.1 → 0.64.1 (plus transitive updates).
- PNG decoder `png` 0.17.16 → 0.18.1.
- ISO helpers `hadris-fat` / `hadris-udf` 2.4.0 → 2.5.0.
- Runtime `tokio` 1.53.1 → 1.53.2.
- Portable QEMU `qemu-v11.1.1-48` → `qemu-v11.1.2` (SHA-256 pins refreshed).
- virtio-win driver pack `drivers-20260925` → `drivers-20261005` (identical ISO bytes, tag-only bump).

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
