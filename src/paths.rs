//! Single source of truth for every well-known file and cache-group name:
//! installer artifacts, per-VM state files, portable-tree layout, cache
//! groups. Renaming means editing here — a literal for one of these names
//! anywhere else silently splits old/new state (a renamed file the code
//! can't find, a cache group that orphans gigabytes).
//!
//! Deliberately NOT here: socket names (already in [`crate::ipc`]),
//! version pins ([`crate::pins`]), guest-arch mappings like firmware and
//! OpenSSH zip names (data belonging with the arch table in
//! [`crate::unattend`]), and `include_str!` source paths (must stay
//! literal). Tests keep literals on purpose: they guard these values.
//! Help text in `main.rs` repeats names for humans; those are docs.

// --- installer inputs ---------------------------------------------------
/// VirtIO driver ISO (vendored release asset, ISO cache, USB staging).
pub const VIRTIO_ISO: &str = "virtio-win.iso";
/// Hash sidecar next to the virtio release asset.
pub const VIRTIO_ISO_SHA256: &str = "virtio-win.iso.sha256";
/// Windows 11 retail ISO filename used by the `fetch` chain.
/// Proven media is retail (`Windows11_Client_*`); non-retail
/// (Enterprise/OEM) media has never been exercised (see README "Not tested").
pub const WIN11_ISO_NAME: &str = "Windows11_Client.iso";
/// Answer file at the installer USB root and in `gen` output dirs.
pub const UNATTEND_XML: &str = "autounattend.xml";
/// Partition script inside the installer USB tree.
pub const PART_TXT_PATH: &str = "setup/part.txt";
/// Driver/tools dir inside the installer USB tree.
pub const TOOLS_DIR: &str = "tools";
/// Stage subdir for an explicit `--drivers-dir` tree.
pub const DRIVERS_STAGE_DIR: &str = "drivers";

// --- per-VM state (inside the VM dir) ------------------------------------
/// Boot disk `run` boots and `install` writes.
pub const DISK_IMAGE: &str = "disk.qcow2";
/// Guest NVRAM copy, seeded from the portable firmware template.
pub const VARS_FILE: &str = "VARS.fd";
/// QEMU pidfile: written on launch, removed by `stop`.
pub const QEMU_PIDFILE: &str = "qemu.pid";
/// Installer QEMU's stdout/stderr capture.
pub const INSTALL_LOG: &str = "install.log";
/// Live-snapshot companions (all inside the VM dir, all removed when an
/// install voids them): migrated RAM + device state, the VARS backup taken
/// at the same instant (raw pflash is outside every snapshot), and the
/// overlay that has held post-save writes since the pivot.
pub const LIVE_STATE_FILE: &str = "golden-live.state";
pub const LIVE_VARS_BACKUP: &str = "VARS.fd.golden-live";
pub const LIVE_OVERLAY: &str = "disk-live-overlay.qcow2";

// --- cache layout (group names under the cache root) ---------------------
pub const QEMU_GROUP: &str = "qemu-portable";
pub const WIMLIB_GROUP: &str = "wimlib-portable";
pub const ISO_GROUP: &str = "iso";
pub const VM_GROUP: &str = "vms/win11";
pub const MEDIA_GROUP: &str = "media";
/// USB installer image inside the media group.
pub const WINUSB_IMAGE: &str = "winusb.dmg";

// --- no-downloader fallbacks (local relative paths) ----------------------
pub const FALLBACK_VM_DIR: &str = "./win11-vm";
pub const FALLBACK_WINUSB_IMAGE: &str = "./winusb.dmg";
pub const FALLBACK_UNATTEND_DIR: &str = "./unattend_dir";
pub const FALLBACK_QEMU_CACHE: &str = ".cache/qemu-portable";

// --- portable-tree layout -------------------------------------------------
/// Extracted tree dir name (`<group>/<tag>/qemu-portable`), for QEMU…
pub const QEMU_INNER: &str = "qemu-portable";
/// …and wimlib.
pub const WIMLIB_INNER: &str = "wimlib-portable";
/// Binary dir inside an extracted tree.
pub const PORTABLE_BIN_DIR: &str = "bin";
/// `qemu-img` binary name (sentinel proving a QEMU tree is extracted).
pub const QEMU_IMG_NAME: &str = "qemu-img";
/// `wimlib-imagex` binary name (sentinel proving a wimlib tree is extracted).
pub const WIMLIB_IMGX_NAME: &str = "wimlib-imagex";

// --- release asset conventions --------------------------------------------
/// Asset name prefix selecting this host's QEMU archive in a release.
pub const QEMU_ASSET_PREFIX: &str = "qemu-portable-";
/// Asset name prefix selecting this host's wimlib archive in a release.
pub const WIMLIB_ASSET_PREFIX: &str = "wimlib-portable-";
/// Host-asset markers selecting the per-platform archive (and its pinned
/// hash in [`crate::pins`]). Single source of truth: `asset_for_host` and
/// the pin lookups both match on these — never retype the literals.
pub const MARKER_MACOS_ARM64: &str = "macos-arm64";
pub const MARKER_LINUX_X86_64: &str = "linux-x86_64";
pub const MARKER_LINUX_AARCH64: &str = "linux-aarch64";
/// Checksum sheet published next to release assets.
pub const SHA256SUMS_FILE: &str = "SHA256SUMS.txt";
