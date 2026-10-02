# windows-headless

Build Windows 11 VM images and run them headless. Give it a Microsoft ISO ([amd64](https://www.microsoft.com/en-us/software-download/windows11) / [arm64](https://www.microsoft.com/en-us/software-download/windows11arm64)), it builds the installer, boots the VM, and hands you SSH. One binary, no server, no GUI.

**STATUS EXPERIMENTAL: [tested](#tested) on macOS (Apple Silicon) and Linux (amd64 + arm64) hosts. Intel Macs are not supported. macOS users: see [Gatekeeper requirement](#macos-gatekeeper-requirement).**

```sh
# Build the installer image from your ISO (downloads required tools on first run)
cargo run -- build \
  --win-iso ~/Downloads/Win11.iso

# Boot the VM and wait for SSH
cargo run -- run

# Or build and boot in one command
cargo run -- run \
  --win-iso ~/Downloads/Win11.iso
```

Hidden debug tier (`install`, `setup`, `build-media`, `fetch-*`, `gen`, `report`, `overlay`, `stop`, `guest`): omitted from `--help`, invocable by name for stage-by-stage forensics.

> Also a Rust library: the CLI is thin, and `VmConfig`, `HeadlessVmBuilder`, `DiskManager`, and friends are all exported for programmatic use from your own projects.

## Tested

Proven installs, logged automatically by `install` to [tested-isos.jsonl](./tested-isos.jsonl) (`*` = built from a dirty tree). Regenerate with `cargo run -- report --readme README.md`.

<!-- TESTED-BEGIN -->
| Date | ISO | Arch | Accel | Host | QEMU | wimlib | virtio | Commit | Result |
|---|---|---|---|---|---|---|---|---|---|
| 2026-10-01 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [29b32fc](https://github.com/jzombie/windows-headless/commit/29b32fc96e3d) | ssh-banner |
<!-- TESTED-END -->

## Not tested

Proven so far: English retail ISO on macOS Apple Silicon, linux/amd64, and linux/arm64 (see table above). Everything below is untested. It may work, fail loudly, or fail silently. Reports with logs welcome.

- **Non-English installs.** Only `en-US` media has been tested. The `run --win-iso` / `install --win-iso` chain rebuilds media as `en-US`, the answer file selects the `Windows 11 Pro` image name, and parts of guest verification parse English command output.
- **Other hosts.** `macos / aarch64`, `linux / amd64`, and `linux / aarch64` have logged installs (see table above). The `linux / aarch64` runs to date were evaluated inside an OrbStack Linux VM (nested virtualization), not on bare metal. Accel is recorded per run in the table.
- **No mixed archs.** Guest arch must match host arch (ARM Windows on macOS Apple Silicon, x64 Windows on x64 Linux). HVF/KVM require it. Cross-arch guests (x64 Windows on macOS) are untested and unsupported; they fall back to TCG emulation at a 5x to 15x slowdown if they boot at all.
- **Eval / Enterprise / OEM media.** Proven installs used retail ISOs (see table above). Eval, Enterprise, and OEM media have never been exercised against this [answer file](./templates/autounattend.xml).

## Download Integrity & Security

The provisioning pipeline downloads third-party binaries. Hash verification is enforced as follows:

| Artifact | Source | Hash Enforced? | Security Guarantee |
|---|---|---|---|
| **`qemu-portable`** & **`wimlib-portable`** | Auto-download ([qemu](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) · [wimlib](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **`virtio-win.iso`** | Auto-download ([drivers](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **OpenSSH Archive** | Auto-download ([releases](https://github.com/PowerShell/Win32-OpenSSH/releases/tag/10.0.0.0p2-Preview)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **Windows 11 ISO** | User-supplied | **NO (Opt-in)** | Microsoft URLs rotate. Validated via TLS and ISO9660 magic bytes only, unless pinned explicitly via `--win-iso-sha256`. |

*Security Note:* For all auto-downloaded components, the trust root is the source code (`src/pins.rs`). The tool will immediately abort if a downloaded payload does not match its hardcoded hash. If you choose to override default downloads using custom URLs (such as `--virtio-iso-url`), those payloads rely solely on TLS unless you explicitly provide their matching hash flags.

Run `cargo run --bin windows-headless -- verify-pins` to check every pin without installing anything. It covers all platforms (macos-arm64, linux-x86_64, linux-aarch64) from any single runner: release metadata is platform-independent, so one CI job verifies every arch.

### macOS Gatekeeper Requirement

macOS hosts are fully supported.

One-time setup: the portable QEMU and wimlib binaries are adhoc-signed (not notarized), so Gatekeeper blocks them until the quarantine flag is cleared from the cache directory.

Either set `WINDOWS_HEADLESS_ALLOW_QUARANTINE_STRIP=1` to let provisioning clear it automatically, or run `xattr -dr com.apple.quarantine <cache-dir>` yourself once. Without one of these, provisioning stops with an error before downloading anything (fail-closed by design, not a malfunction).

## License

`windows-headless` is primarily distributed under the terms of both the [MIT license](./LICENSE-MIT) and the [Apache License (Version 2.0)](./LICENSE-APACHE).
