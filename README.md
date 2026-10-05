# windows-headless

Build Windows 11 VM images and run them headless. Give it a Microsoft ISO ([amd64](https://www.microsoft.com/en-us/software-download/windows11) / [arm64](https://www.microsoft.com/en-us/software-download/windows11arm64)), it builds the installer, boots the VM, and hands you SSH. One command, no server, no GUI, no host packages.

**STATUS EXPERIMENTAL: [Tested](#tested) on macOS (Apple Silicon) and Linux (amd64 + arm64) hosts. Intel Macs are not supported. macOS users: see [Gatekeeper requirement](#macos-gatekeeper-requirement).**

> Prerequisite: A recent Rust toolchain (`cargo`, 1.85+; any stable via
> [rustup](https://rustup.rs)). Everything else (QEMU, wimlib, virtio
> drivers, OpenSSH) is fetched automatically, so a vanilla host with
> just Rust is enough. No prebuilt binaries yet: every command below
> builds from source first.
>
> **Highly recommended:** Run your first install with all defaults (no overridden
> `--memory`, `--cpus`, `--username`, `--password`, etc.). A green
> default install proves the pipeline works on your machine; only then
> start tuning. Custom values on a first run entangle your settings
> with setup problems and make failures much harder to diagnose. See [Where things live](#where-things-live) in this README for where artifacts are stored.

```sh
# One command: build the installer, boot the VM, land at a Command Prompt.
# (macOS first run: prefix with WINDOWS_HEADLESS_ALLOW_QUARANTINE_STRIP=1)
cargo run -- run --win-iso ~/Downloads/Win11.iso

# ...or in two steps: build once, boot any time.
cargo run -- build --win-iso ~/Downloads/Win11.iso
cargo run -- run
```

Sizing is Docker-style: `--memory 8g --cpus 8 --disk-size 128g`
(defaults `4g`, `4`, `64g`). Bare digits keep the old integer meaning
(MB for memory, GB for disk); floors are 2g RAM and 32g disk.
`--smp` still works as an alias of `--cpus`.
Sizing applies to installs too, so `run --win-iso` honors the same flags.

Monitoring: watch the installer live at `vnc://127.0.0.1:5910` (password
`demo2026`); a screenshot lands in `<cache>/vms/win11/screenshots/` every
60 seconds until SSH answers — installs as a time-lapse, `run` boots as a
boot time-lapse (`--screenshots-every-secs`, `0` disables). Boot failures
end with a diagnosis block: latest frame path + screen state (black /
solid color field / modal error dialog / content) + last serial line, so
dead boots are readable without VNC. `black` means literally no visible
pixel — text or a spinner on black is content. The classifier never
claims "error" from color shape alone: the main Windows Setup progress
screen is itself a flat saturated blue, so a flat frame only counts when
nothing at all is rendered on it (≥99.5% one color, <0.1% white, no UI),
and it says `solid color field`, not "error". Black and flat-field
verdicts only stand while the frame is static — animated screens demote
to content (spinner/percent).
Firmware reboots are counted from the serial log (OVMF `BdsDxe: starting`
handoffs): `guest rebooted (firmware boot #N seen on serial)` lines print
while waiting for SSH, and failure reports carry `guest boots seen`.
Installer screenshots that catch a modal failure dialog print a
`WARNING:` line as soon as it appears.

## Where things live

Everything lives under one cache root, announced at startup as
`cache: <path> (<source>)`. Resolution order: `$WINDOWS_HEADLESS_*_DIR`
env override, then `<project>/.cache` (project discovery), then the OS
user cache dir. Layout:

```text
<cache>/
  qemu-portable/<tag>/...   # portable QEMU tree (bin/qemu-system-*, qemu-img)
  wimlib-portable/<tag>/... # wimlib-imagex used to build installer media
  iso/                      # downloaded virtio-win.iso + OpenSSH zips (+ .sha256)
  media/winusb.dmg          # built installer image (+ .json provenance sidecar)
  vms/win11/                # the VM: disk.qcow2, VARS.fd, qemu.pid,
                            # install.log (QEMU output), screenshots/*.png
```

Per-group overrides: `$WINDOWS_HEADLESS_QEMU_DIR`, `$WINDOWS_HEADLESS_ISO_DIR`,
`$WINDOWS_HEADLESS_MEDIA_DIR`, `$WINDOWS_HEADLESS_VM_DIR`.

## Drivers

One third-party driver, installed from the pinned virtio-win ISO:

- **Network: Red Hat VirtIO NetKVM** (`NetKVM/w11/<arch>`). The guest NIC
  is `virtio-net-pci`, so without it there is no network (and no SSH).

Everything else is inbox Windows:

- **Storage: none.** The disk is NVMe, driven by Windows' built-in
  `stornvme`. No `viostor` needed or installed.
- **GPU: none.** Display is `ramfb` (a plain framebuffer for VNC and
  installer screenshots). No virtio-gpu, QXL, or vendor GPU driver: expect
  software rendering only.

Hidden debug tier (`install`, `setup`, `build-media`, `fetch-*`, `gen`, `report`, `overlay`, `stop`, `guest`): omitted from `--help`, invocable by name for stage-by-stage forensics.

> Also a Rust library: the CLI is thin, and `VmConfig`, `HeadlessVmBuilder`, `DiskManager`, and friends are all exported for programmatic use from your own projects.

## Tested

Proven installs, logged automatically by `install` to [tested-isos.jsonl](./tested-isos.jsonl) (`*` = built from a dirty tree). Regenerate with `cargo run -- report --readme README.md`.

<!-- TESTED-BEGIN -->
| Date | ISO | Arch | Accel | Host | QEMU | wimlib | virtio | Commit | Result |
|---|---|---|---|---|---|---|---|---|---|
| 2026-10-05 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [d626a29](https://github.com/jzombie/windows-headless/commit/d626a29d0abf) | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | 03d08e5* | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [c00c958](https://github.com/jzombie/windows-headless/commit/c00c95839c56) | ssh-banner |
| 2026-10-04 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [c369131](https://github.com/jzombie/windows-headless/commit/c36913109b1d) | ssh-banner |
| 2026-10-02 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [51606bf](https://github.com/jzombie/windows-headless/commit/51606bf345ba) | ssh-banner |
| 2026-10-02 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [c077bbc](https://github.com/jzombie/windows-headless/commit/c077bbcff71d) | ssh-banner |
| 2026-10-02 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | tcg | linux Ubuntu 26.04 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [088e297](https://github.com/jzombie/windows-headless/commit/088e297df38f) | ssh-banner |
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

One-time setup: the [portable QEMU and wimlib binaries](https://github.com/jzombie/lab-qemu-portable/releases) are adhoc-signed (not notarized), so Gatekeeper blocks them until the quarantine flag is cleared from the cache directory.

Either set `WINDOWS_HEADLESS_ALLOW_QUARANTINE_STRIP=1` to let provisioning clear it automatically, or run `xattr -dr com.apple.quarantine <cache-dir>` yourself once. Without one of these, provisioning stops with an error before downloading anything (fail-closed by design, not a malfunction).

## License

`windows-headless` is primarily distributed under the terms of both the [MIT license](./LICENSE-MIT) and the [Apache License (Version 2.0)](./LICENSE-APACHE).
