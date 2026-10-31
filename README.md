# windows-headless

Build Windows 11 VM images and run them headless. Input: Microsoft retail ISO ([amd64](https://www.microsoft.com/en-us/software-download/windows11) / [arm64](https://www.microsoft.com/en-us/software-download/windows11arm64)). Output: SSH shell. One command provisions a real VM in-process. No daemon, no GUI, no host packages, no Docker, no nested virtualization.

**STATUS: EXPERIMENTAL.** Tested on macOS (Apple Silicon) and Linux (amd64, arm64): see [Tested](#tested). Intel Macs unsupported. Windows hosts untested. See [Troubleshooting](#troubleshooting) on failure.

> Requires a Rust toolchain (`cargo` 1.85+ via [rustup](https://rustup.rs)).
> Remaining dependencies download automatically on first run and execute from
> the cache. Host stays clean. Source-only distribution: commands below build
> via `cargo` first.
>
> First install: use all defaults. A green default install separates pipeline
> faults from configuration faults. Tune after. Artifact locations:
> [Where things live](#where-things-live).

QEMU, wimlib, and virtio-win builds come from
[lab-qemu-portable](https://github.com/jzombie/lab-qemu-portable): portable
zero-install binaries for Windows, Linux, and macOS. Downloaded and
SHA-256-verified into the cache on first run.

```sh
# One command: build the installer, boot the VM, land at a Command Prompt.
# Always --release. Debug builds slow every status tick.
cargo run --release -- run --win-iso <PATH_TO_WIN_ISO>

# Or split: build once, boot on demand.
cargo run --release -- build --win-iso <PATH_TO_WIN_ISO>
cargo run --release -- run
```

Docker-style sizing: `--memory 8g --cpus 8 --disk-size 128g`
(defaults `4g`, `4`, `64g`). Bare digits use legacy units (memory MB, disk
GB). Floors: 2g RAM, 32g disk. `--smp` aliases `--cpus`. Applies to installs;
`run --win-iso` honors the same flags.

Connect:
- SSH: `ssh -p 2232 AdminUser@127.0.0.1`, password `P@ssw0rd123!`
  (`--username` / `--password` override; the CLI prints the exact
  command when ready).
- VNC: `vnc://127.0.0.1:5910`, password `demo2026`.

Watch:
- Screenshots to `<cache>/vms/win11/screenshots/` every 60s
  (`--screenshots-every-secs`, `0` disables).
- In-memory screen classification between saves: nearest-exemplar match every
  10s on hardware accel, 30s on TCG (`--classify-every-secs`, `0` disables).
  States: installer progress, OOBE, firmware boot art, lock/idle, plus error
  states (modal dialog, UEFI shell, recovery).
- Alerts require error state AND idle guest (no disk I/O, no growth). Active
  guests never alert. Classification misreads; pixel-change motion sensing
  tracks aliveness independently.
- Dead boots terminate with a diagnosis block: latest frame path, screen
  state, last serial line, firmware reboot count.

## Where things live

Single cache root, reported at startup as `cache: <path> (<source>)`.
Resolution: `$WINDOWS_HEADLESS_*_DIR` override, `<project>/.cache`, OS user
cache dir. Layout:

```text
<cache>/
  qemu-portable/<tag>/...   # portable QEMU tree (bin/qemu-system-*, qemu-img)
  wimlib-portable/<tag>/... # wimlib-imagex used to build installer media
  iso/                      # downloaded virtio-win.iso + OpenSSH zips (+ .sha256)
  media/winusb.dmg          # built installer image (+ .json provenance sidecar)
  vms/win11/                # the VM: disk.qcow2 (+ .golden sidecar),
                            # VARS.fd, qemu.pid, install.log (QEMU output),
                            # screenshots/*.png; live snapshot files when saved:
                            # golden-live.state, VARS.fd.golden-live,
                            # disk-live-overlay.qcow2, disk.qcow2.golden-live
```

Per-group overrides: `$WINDOWS_HEADLESS_QEMU_DIR`, `$WINDOWS_HEADLESS_ISO_DIR`,
`$WINDOWS_HEADLESS_MEDIA_DIR`, `$WINDOWS_HEADLESS_VM_DIR`.

Uninstall: delete the cache root. Hypervisor, disk, snapshots, media, and
downloads all live under it. Host untouched: no packages, drivers, or
services. (With per-group overrides set, delete each directory instead.)

## Drivers

One third-party driver, from the pinned virtio-win ISO. Installed automatically
during Setup: the specialize pass runs `pnputil /add-driver ... /install` off
the staged media.

- **Network: Red Hat VirtIO NetKVM** (`NetKVM/w11/<arch>`). Guest NIC is
  `virtio-net-pci`; without this driver the guest has no network, hence no SSH.

Remainder inbox:

- **Storage: none.** NVMe disk on Windows' built-in `stornvme`. No `viostor`.
- **GPU: none.** `ramfb` framebuffer for VNC and screenshots. No virtio-gpu,
  QXL, or vendor driver. Software rendering only.

No automated driver upgrades. New drivers require a media rebuild and a
reinstall from scratch.

Hidden debug tier (`install`, `setup`, `build-media`, `fetch-*`, `gen`, `report`, `overlay`, `stop`, `guest`): absent from `--help`, invocable by name for stage-by-stage forensics.

> Also a Rust library: thin CLI over exported `VmConfig`, `HeadlessVmBuilder`, `DiskManager`.

## Snapshots

Golden rewind is the default. First golden snapshot is automatic: `build`
(and `run --win-iso` on a missing disk) verifies the shell, then saves disk
state. Every later `run` rewinds first. Dirty stops (Ctrl-C) cannot leave the
next boot in Recovery.

```sh
# Terminal 1: `run` blocks, so boot and leave it running.
cargo run --release -- run
```

```sh
# Terminal 2: re-take golden from the running guest (verifies shell, clean
# shutdown, snapshot). Next `run` resumes the new state.
cargo run --release -- snapshot
```

Live resume (experimental) also saves RAM: `run --live` restores the
running guest with no boot.

```sh
# Terminal 1: boot and leave it running.
cargo run --release -- run
```

```sh
# Terminal 2: save live state (brief guest stun; guest keeps running).
cargo run --release -- snapshot --live
```

```sh
# Stop the guest first (Ctrl-C terminal 1, or `stop`), then resume with no boot.
# A missing piece (overlay, VARS backup, migration stream) fails loudly
# rather than booting a half-state.
cargo run --release -- run --live
```

Golden and live coexist as separate sidecars; `run` selects by flag. Plain
`run` rewinds to golden and loudly drops orphaned live overlays. `run --live`
skips the rewind and requires every live piece, else refuses. New installs
void both. Re-taking overwrites that type only. One `snapshot` at a time
(the [QMP](https://www.qemu.org/docs/master/interop/qmp-spec.html) control
channel admits a single client).

Storage: golden is one named snapshot inside `disk.qcow2` (re-taking replaces
it; nothing accumulates). Live state is separate files beside the disk,
consumed or dropped on resume, never merged. Filenames:
[Where things live](#where-things-live).

Rewind is CPU-bound (firmware + Windows boot); live is I/O-bound (2-3 GB
stream + settle). Apple Silicon, cache on external USB: rewind wins (~19s
spawn-to-SSH vs ~23s live; fast boot, slow USB stream). Older Intel i7,
internal SSD: live wins (slow boot, fast stream). Live costs multi-GB
sidecars. Benchmark `run` vs `run --live` on your hardware; default to rewind
unless live measures faster.

## Tested

Proven installs, logged automatically by `install` to [tested-isos.jsonl](./tested-isos.jsonl) (`*` = built from a dirty tree). Regenerate with `cargo run --release -- report --readme README.md`.

<!-- TESTED-BEGIN -->
| Date | ISO | Arch | Accel | Host | QEMU | wimlib | virtio | Commit | Result |
|---|---|---|---|---|---|---|---|---|---|
| 2026-10-31 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 24.04.5 LTS | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [523cd19](https://github.com/jzombie/windows-headless/commit/523cd196d6c5) | ssh-banner |
| 2026-10-30 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 24.04.5 LTS | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [e99a722](https://github.com/jzombie/windows-headless/commit/e99a722fca00) | ssh-banner |
| 2026-10-25 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 24.04.5 LTS | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [1970285](https://github.com/jzombie/windows-headless/commit/197028595f20) | ssh-banner |
| 2026-10-25 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [eb8575e](https://github.com/jzombie/windows-headless/commit/eb8575e51b04) | ssh-banner |
| 2026-10-12 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [77c796b](https://github.com/jzombie/windows-headless/commit/77c796b0466f) | ssh-banner |
| 2026-10-12 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 24.04.5 LTS | [qemu-v11.1.2](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.2) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [f726b22](https://github.com/jzombie/windows-headless/commit/f726b22443db) | ssh-banner |
| 2026-10-12 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 24.04.5 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [fd9cde7](https://github.com/jzombie/windows-headless/commit/fd9cde705551) | ssh-banner |
| 2026-10-08 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [16e14b0](https://github.com/jzombie/windows-headless/commit/16e14b08c4a1) | ssh-banner |
| 2026-10-08 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | 8fa0b0e* | ssh-banner |
| 2026-10-08 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | 2247379* | ssh-banner |
| 2026-10-07 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [378eccf](https://github.com/jzombie/windows-headless/commit/378eccf79a23) | ssh-banner |
| 2026-10-06 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [20f689f](https://github.com/jzombie/windows-headless/commit/20f689f26d89) | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | tcg | linux Ubuntu 26.04 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [20f689f](https://github.com/jzombie/windows-headless/commit/20f689f26d89) | ssh-banner |
| 2026-10-05 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [d626a29](https://github.com/jzombie/windows-headless/commit/d626a29d0abf) | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | 03d08e5* | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [c00c958](https://github.com/jzombie/windows-headless/commit/c00c95839c56) | ssh-banner |
| 2026-10-04 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [c369131](https://github.com/jzombie/windows-headless/commit/c36913109b1d) | ssh-banner |
| 2026-10-02 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [51606bf](https://github.com/jzombie/windows-headless/commit/51606bf345ba) | ssh-banner |
| 2026-10-02 | Windows11_Client_x64_en-us_26300_9457.iso | amd64 | kvm | linux Ubuntu 26.04.1 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [c077bbc](https://github.com/jzombie/windows-headless/commit/c077bbcff71d) | ssh-banner |
| 2026-10-02 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | tcg | linux Ubuntu 26.04 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [088e297](https://github.com/jzombie/windows-headless/commit/088e297df38f) | ssh-banner |
| 2026-10-01 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20261005) | [29b32fc](https://github.com/jzombie/windows-headless/commit/29b32fc96e3d) | ssh-banner |
<!-- TESTED-END -->

## Not tested

Proven: English retail ISO on macOS Apple Silicon, linux/amd64, linux/arm64 (table above). Below is untested. It may work, fail loudly, or fail silently. Reports with logs welcome.

- **Non-English installs.** Only `en-US` media tested. The install chain rebuilds media as `en-US`, the answer file selects the `Windows 11 Pro` image, and guest verification parses English command output.
- **Other hosts.** Logged installs exist for `macos / aarch64`, `linux / amd64`, `linux / aarch64` (table above). The `linux / aarch64` runs executed inside an OrbStack Linux VM (nested), not bare metal. Accel recorded per run.
- **Windows hosts.** No proven install on a Windows host (table above). The portable tree ships a Windows QEMU build; the install path there is unexercised.
- **No mixed archs.** Guest arch matches host arch (ARM Windows on Apple Silicon, x64 Windows on x64 Linux). HVF/KVM require it. Cross-arch guests (x64 Windows on macOS) are untested and unsupported: TCG fallback at 5x to 15x slowdown, if they boot.
- **Non-retail media.** Proven installs used retail ISOs (table above). Enterprise, OEM, and other non-retail media never exercised against this [answer file](./templates/autounattend.xml).

## Download Integrity & Security

Provisioning downloads third-party binaries. Hash enforcement:

| Artifact | Source | Hash Enforced? | Security Guarantee |
|---|---|---|---|
| **`qemu-portable`** & **`wimlib-portable`** | Auto-download ([releases](https://github.com/jzombie/lab-qemu-portable/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **`virtio-win.iso`** | Auto-download ([releases](https://github.com/jzombie/lab-qemu-portable/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **OpenSSH Archive** | Auto-download ([releases](https://github.com/PowerShell/Win32-OpenSSH/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **Windows 11 ISO** | User-supplied | **NO (Opt-in)** | Microsoft URLs rotate. Validated via TLS and ISO9660 magic bytes only, unless pinned explicitly via `--win-iso-sha256`. |

*Trust root for auto-downloaded components: `src/pins.rs`.* Hash mismatch aborts. Custom URLs (e.g. `--virtio-iso-url`) rely on TLS alone unless hash flags are provided.

Run `cargo run --release --bin windows-headless -- verify-pins` to check every pin without installing. Covers all platforms (macos-arm64, linux-x86_64, linux-aarch64) from any single runner: release metadata is platform-independent, so one CI job verifies every arch.

## Troubleshooting

**macOS Gatekeeper:** If you manually download and extract release archives via a web browser, macOS will quarantine the files. Clear the attributes manually before running: `xattr -dr com.apple.quarantine <cache-dir>` (see [Where things live](#where-things-live) for location of `cache-dir`).

**If SSH prompt is never reached**

- Lockscreen in screenshots with no SSH is normal mid-install. Typical time to SSH on macOS: 10-15 minutes from first boot. Observed outliers to 18 minutes.
- Slow or virtualized hardware (older machines, [OrbStack](https://orbstack.dev/) on macOS) takes considerably longer. After the first stable prompt, take a [live snapshot](#snapshots); subsequent `run --live` restores in seconds.

**Check [Not tested](#not-tested)**

Unsupported platforms, archs, and media live there.

**Something else?**

- Failing on non-default params (custom user/password, sizing)? Re-run defaults as a canary first.
- Otherwise [open an issue](https://github.com/jzombie/windows-headless/issues/new).

## License

`windows-headless` is primarily distributed under the terms of both the [MIT license](./LICENSE-MIT) and the [Apache License (Version 2.0)](./LICENSE-APACHE).
