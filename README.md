# windows-headless

Build Windows 11 VM images and run them headless. Give it a Microsoft ISO ([amd64](https://www.microsoft.com/en-us/software-download/windows11) / [arm64](https://www.microsoft.com/en-us/software-download/windows11arm64)), it builds the installer, boots the VM, and hands you SSH. One command, no server, no GUI, no host packages.

**STATUS EXPERIMENTAL: [Tested](#tested) on macOS (Apple Silicon) and Linux (amd64 + arm64) hosts. Intel Macs are not supported. See [Troubleshooting](#troubleshooting) section should unanticipated problems occur.**

> Prerequisite: A recent Rust toolchain (`cargo`, 1.85+; any stable via
> [rustup](https://rustup.rs)). Everything else ([QEMU, wimlib, virtio
> drivers](https://github.com/jzombie/lab-qemu-portable/releases),
> [OpenSSH](https://github.com/PowerShell/Win32-OpenSSH/releases)) is fetched
> automatically, so a vanilla host with just Rust is enough. No prebuilt
> binaries yet: every command below builds from source first.
>
> **Highly recommended:** Run your first install with all defaults (no overridden
> `--memory`, `--cpus`, `--username`, `--password`, etc.). A green
> default install proves the pipeline works on your machine; only then
> start tuning. Custom values on a first run entangle your settings
> with setup problems and make failures much harder to diagnose. See [Where things live](#where-things-live) in this README for where artifacts are stored.

```sh
# One command: build the installer, boot the VM, land at a Command Prompt.
# --release throughout: debug-build screen classification runs an order of
# magnitude slower and stretches every status tick.
cargo run --release -- run --win-iso <PATH_TO_WIN_ISO>

# ...or in two steps: build once, boot any time.
cargo run --release -- build --win-iso <PATH_TO_WIN_ISO>
cargo run --release -- run
```

Sizing is Docker-style: `--memory 8g --cpus 8 --disk-size 128g`
(defaults `4g`, `4`, `64g`). Bare digits keep the old integer meaning
(MB for memory, GB for disk); floors are 2g RAM and 32g disk.
`--smp` still works as an alias of `--cpus`.
Sizing applies to installs too, so `run --win-iso` honors the same flags.

Monitoring: watch the installer live at `vnc://127.0.0.1:5910` (password
`demo2026`); a screenshot lands in `<cache>/vms/win11/screenshots/` every
60 seconds until SSH answers (`--screenshots-every-secs`, `0` disables).
Between saves, a tiny on-device CNN classifies the screen in memory every
10s on hardware accel (30s on TCG; `--classify-every-secs`, `0` disables),
so phase changes and error screens surface in the log within seconds.
Dead boots end with a diagnosis block: latest frame path, screen state, last
serial line, and firmware reboot count, so dead boots are readable without
VNC. Screen states: installer progress, first-boot OOBE, firmware boot art,
lock/idle, plus error states (modal error dialog, UEFI shell with no
bootable device found, Windows recovery).
Error states print a `WARNING:` line only when the guest is also flat
(no disk I/O, no disk growth) — a moving guest with an error read logs as
observation, never alarm.
The classifier is eyes into the installer's progress, not ground truth:
it misreads, and warnings fire on the activity conjunction, never on a
single frame.

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
  vms/win11/                # the VM: disk.qcow2 (+ .golden sidecar),
                            # VARS.fd, qemu.pid, install.log (QEMU output),
                            # screenshots/*.png; live snapshot files when saved:
                            # golden-live.state, VARS.fd.golden-live,
                            # disk-live-overlay.qcow2, disk.qcow2.golden-live
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

> There is no automated driver upgrade path. To pick up newer drivers, rebuild
the installer and reinstall from scratch with the latest media.

Hidden debug tier (`install`, `setup`, `build-media`, `fetch-*`, `gen`, `report`, `overlay`, `stop`, `guest`): omitted from `--help`, invocable by name for stage-by-stage forensics.

> Also a Rust library: the CLI is thin, and `VmConfig`, `HeadlessVmBuilder`, `DiskManager`, and friends are all exported for programmatic use from your own projects.

## Snapshots

Golden rewind is the default. The first golden snapshot is automatic: `build`
(and likewise `run --win-iso`, which installs when the disk is missing) proves
the shell, then saves that disk state. Every `run` afterward rewinds to it
first, so a dirty stop (Ctrl-C) can't leave the next boot in Recovery.

```sh
# Terminal 1: `run` blocks, so boot and leave it running.
cargo run --release -- run
```

```sh
# Terminal 2: re-take golden from the running guest (proves shell,
# shuts down cleanly, snapshots). The next `run` resumes the new state.
cargo run --release -- snapshot
```

Live resume (experimental) also saves RAM, so `run --live` restores the
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

Golden and live coexist (separate sidecars) and `run` selects by flag, so you
can alternate between them. Plain `run` rewinds to golden and drops any
orphaned live overlay out loud; `run --live` skips the rewind and requires
every live piece or refuses. A new install voids both. Re-taking either
overwrites that type only; run one `snapshot` at a time (the [QMP](https://www.qemu.org/docs/master/interop/qmp-spec.html) control
channel to the running VM allows a single client).

Storage: golden is a single named snapshot inside `disk.qcow2` (re-taking it
replaces it, so nothing accumulates). Live state is separate files beside the
disk that are consumed or dropped on resume, never merged. See
[Where things live](#where-things-live) for the exact filenames.

Rewind is CPU-bound (full firmware + Windows boot); live is I/O-bound (2–3 GB
stream + settle). On Apple Silicon with the cache on external USB, rewind wins:
~19s spawn-to-SSH vs ~23s live, since the boot is fast and the USB stream is
slow. On an older Intel i7 with an internal SSD the balance flips: the boot is
much slower while the 2–3 GB stream off SSD is fast, so live wins. Live also
costs multi-GB sidecars. Time `run` vs `run --live` on your hardware; default
to the rewind unless live is faster.

## Tested

Proven installs, logged automatically by `install` to [tested-isos.jsonl](./tested-isos.jsonl) (`*` = built from a dirty tree). Regenerate with `cargo run --release -- report --readme README.md`.

<!-- TESTED-BEGIN -->
| Date | ISO | Arch | Accel | Host | QEMU | wimlib | virtio | Commit | Result |
|---|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [378eccf](https://github.com/jzombie/windows-headless/commit/378eccf79a23) | ssh-banner |
| 2026-10-06 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | hvf | macos 26.6.2 | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [20f689f](https://github.com/jzombie/windows-headless/commit/20f689f26d89) | ssh-banner |
| 2026-10-05 | Windows11_Client_arm64_en-us_26300_9457.iso | arm64 | tcg | linux Ubuntu 26.04 LTS | [qemu-v11.1.1-48](https://github.com/jzombie/lab-qemu-portable/releases/tag/qemu-v11.1.1-48) | [wimlib-v1.14.5-39](https://github.com/jzombie/lab-qemu-portable/releases/tag/wimlib-v1.14.5-39) | [303f7ae](https://github.com/jzombie/lab-qemu-portable/releases/tag/drivers-20260925) | [20f689f](https://github.com/jzombie/windows-headless/commit/20f689f26d89) | ssh-banner |
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
| **`qemu-portable`** & **`wimlib-portable`** | Auto-download ([releases](https://github.com/jzombie/lab-qemu-portable/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **`virtio-win.iso`** | Auto-download ([releases](https://github.com/jzombie/lab-qemu-portable/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **OpenSSH Archive** | Auto-download ([releases](https://github.com/PowerShell/Win32-OpenSSH/releases)) | **YES (Pinned)** | The downloaded bytes are strictly verified against hardcoded SHA-256 pins in `src/pins.rs`. |
| **Windows 11 ISO** | User-supplied | **NO (Opt-in)** | Microsoft URLs rotate. Validated via TLS and ISO9660 magic bytes only, unless pinned explicitly via `--win-iso-sha256`. |

*Security Note:* For all auto-downloaded components, the trust root is the source code (`src/pins.rs`). The tool will immediately abort if a downloaded payload does not match its hardcoded hash. If you choose to override default downloads using custom URLs (such as `--virtio-iso-url`), those payloads rely solely on TLS unless you explicitly provide their matching hash flags.

Run `cargo run --release --bin windows-headless -- verify-pins` to check every pin without installing anything. It covers all platforms (macos-arm64, linux-x86_64, linux-aarch64) from any single runner: release metadata is platform-independent, so one CI job verifies every arch.

## Troubleshooting

**macOS Gatekeeper:** If you manually download and extract release archives via a web browser, macOS will quarantine the files. Clear the attributes manually before running: `xattr -dr com.apple.quarantine <cache-dir>`.

**If SSH prompt is never reached**

- If the installer screenshots show that you're sitting on a lockscreen and SSH was never reached, it *should* eventually come online.  Typically, on macOS, SSH will be reached within 10 - 15 minutes of QEMU first booting and starting the Windows install, but I've seen it wait for 18 minutes (this is not typical).
- On non-virtualized, or older, hardware (including running in [OrbStack](https://orbstack.dev/) on macOS), it can take *much longer* to reach that initial stable prompt.  Once it *is* reached, saving a [live snapshot](#snapshots) is **highly recommended** and a subsequent launch via `run --live` should load that live snapshot within seconds.

**Refer to what hasn't yet been tested**

The [not tested](#not-tested) section shows some platforms/archs/releases/spins that have not yet been tested.

## License

`windows-headless` is primarily distributed under the terms of both the [MIT license](./LICENSE-MIT) and the [Apache License (Version 2.0)](./LICENSE-APACHE).
