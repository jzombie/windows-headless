# windows-headless

Minimal-dependency headless Windows 11 VM provisioning: download Eval +
VirtIO ISOs, render a hardened `autounattend.xml` into a vvfat folder,
boot QEMU headless, poll SSH.

## Quick start

```bash
# One-time: portable QEMU + VirtIO drivers into ./.cache (no system QEMU needed)
cargo run -- fetch-qemu
cargo run -- fetch          # skips Eval ISO if you already have one on disk

# Install Windows 11 ARM64 with a visible window, everything in ./.cache
cargo run -- run \
  --win-iso ~/Downloads/Win11_25H2_English_Arm64_v2.iso \
  --virtio-iso ./.cache/iso/virtio-win.iso \
  --display cocoa \
  --qemu-dir ./.cache/qemu-portable/<version>/qemu-portable

# Later: SSH in (self-installed, no desktop login needed)
ssh -o StrictHostKeyChecking=no -p 2222 AdminUser@127.0.0.1 "cmd.exe /c ver"
```

`run` defaults the disk to `./.cache/vms/win11/disk.qcow2` and the answer
file to `./.cache/vms/win11/unattend/autounattend.xml` (override with
`--disk`, `--unattend-dir`, `--vm-dir`). Tune with `--memory 8192 --smp 8`.
`run` auto-answers the ISO's "press any key to boot from CD" prompt via
QMP `send-key` (two bounded taps); stale monitor/QMP socket files are
unlinked on launch so reruns don't fail to bind.

## Lifecycle (never orphans)

The CLI is a supervisor that owns QEMU: `run`/`boot` block until Ctrl-C /
SIGTERM, then ACPI-shutdown the guest (90s grace, SIGKILL fallback) and
exit. Three layers enforce it: `kill_on_drop(true)` on the child handle
(any drop path kills QEMU — no detach exists), explicit signal handling
(signals don't unwind, so this is what actually covers SIGTERM), and
`<vm-dir>/qemu.pid` + `stop` (QMP `quit`) for VMs from older binaries.
`run` also refuses to start over a live VM or stale disk (`--force`
recreates, refused while alive).

## ISO cache (cache-manager, no eviction)

`fetch` downloads both ISOs into `$WINDOWS_HEADLESS_ISO_DIR/iso`, else
`<project>/.cache/iso` (anchored to the workspace root via discovery, so
plain `cargo run` from this repo caches in `./.cache`, gitignored). Reruns
skip files already present; `--virtio-sha256 <hex>` re-downloads on hash
mismatch. Every file is magic-checked (`CD001`): bot-wall HTML saved as
`.iso` is deleted and reported, never booted. No `EvictPolicy` is ever
applied — no `max_bytes` cap, no LRU.
ISOs are multi-GB with time-limited Eval URLs, so size-capped eviction
would force surprise re-downloads. Downloads land on a `.part` sibling
first, so an interrupted run never leaves a truncated file the cache
mistakes for valid.

If Fedora's bot-check blocks scripted downloads ("Making sure you're not
a bot" saved as `.iso`): open
`https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso`
in a real browser and save it as `./.cache/iso/virtio-win.iso` (or pass
any mirror via `fetch --virtio-iso-url <url>`). `fetch`/`run` validate
whatever is there and keep it without re-downloading.

## Key facts

- Answer file via vvfat: `-drive file=fat:ro:./unattend_dir,format=raw,if=ide`
  (never `if=virtio` — WinPE reads it before vioscsi loads; never modify the
  dir while the VM runs). Opt-in `--unattend-iso` secondary CD-ROM for
  strict CI.
- VirtIO drivers: Fedora **stable-virtio**
  (`fedorapeople.org/.../stable-virtio/virtio-win.iso`), the RedHat-signed
  industry source used by Proxmox/libvirt.
- SSH needs no desktop: `sshd` starts at the lock screen; no `AutoLogon` by
  default (`--autologon` opt-in).
- Debug hangs: `-monitor` + `screendump /tmp/x.ppm`, or swap
  `-display none` → `-display vnc=127.0.0.1:0`.
- Snapshots: golden-master offline overlays (`qemu-img create -b`, clean
  `Stop-Computer -Force` first) for caching; live QMP
  `blockdev-snapshot-sync` / `savevm` for zero-downtime checkpoints.
- Canary: weekly latest-ISO job with 25 min SSH timeout + PPM artifact;
  new screens get bypassed via `docs/oobe-bypass-runbook.md`.
- Eval ISO sourcing: no page scraper (EvalCenter links require the
  interactive form). Paste a fresh time-limited browser link via
  `fetch --win-iso-url` / `WIN11_EVAL_ISO_URL`;
  canary CI uses Fido; Mido.sh is a shell-script alternative.
