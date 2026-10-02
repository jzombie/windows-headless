//! Single source of truth for every external version pin and release
//! identity. Bumping a dependency means editing here — not hunting
//! `format!` strings across modules.
//!
//! QEMU is pinned by full release tag (`QEMU_PORTABLE_TAG`): a floating
//! QEMU once moved VNC password auth under us via a DES-less crypto
//! backend, and rebuild tags (`qemu-vX.Y.Z-<run>`) mean a bare version
//! no longer names a unique build. Explicit `--qemu-tag` still allows
//! testing new builds.

/// GitHub repo hosting our portable releases (QEMU, wimlib, drivers).
pub const RELEASE_REPO: &str = "jzombie/lab-qemu-portable";

// NOTE: the tag is used verbatim.

/// Pinned portable QEMU release, as a full release tag in
/// [`RELEASE_REPO`] (https://github.com/jzombie/lab-qemu-portable) —
/// never an upstream version number. Rebuilds take `qemu-vX.Y.Z-<run>`
/// while the proven tag stays intact, so the tag is the only unambiguous
/// build identity (binaries, bundled libs, firmware as one unit).
/// `run`/`install` use exactly this; `fetch-qemu` downloads exactly this
/// unless `--qemu-tag` overrides for new-build testing.
pub const QEMU_PORTABLE_TAG: &str = "qemu-v11.1.1-48";

/// SHA-256 of the pinned QEMU portable archives at [`QEMU_PORTABLE_TAG`],
/// per host-asset marker (see `MARKER_*` in [`crate::paths`]). Verified
/// 2026-10-01 against the release `SHA256SUMS.txt`.
/// BUMP RULE: a new tag means new bytes — update all three alongside the tag.
pub const QEMU_PORTABLE_SHA256_MACOS_ARM64: &str =
    "5424a4b998a9dddc8b5c67c515e64d0d653f5e786068c86ec6bd70063ad4a9ec";
pub const QEMU_PORTABLE_SHA256_LINUX_X86_64: &str =
    "ebf52e87cd5887c68d14ff5e4dac6549f38a671a37167b517e4502f8b0ac884e";
pub const QEMU_PORTABLE_SHA256_LINUX_AARCH64: &str =
    "530f871ccb7ed6da34367dbe9126ddc6660d2989555d0e913ec3dec179ed42bc";

/// Pinned portable wimlib release, as a full release tag in
/// [`RELEASE_REPO`] — never a bare version. Same contract as QEMU:
/// rebuilds take suffixed tags, so the tag is the only unambiguous
/// build identity. `build-media` uses exactly this; `fetch-wimlib`
/// downloads exactly this unless `--wimlib-tag` overrides.
pub const WIMLIB_PORTABLE_TAG: &str = "wimlib-v1.14.5-39";

/// SHA-256 of the pinned wimlib portable archives at
/// [`WIMLIB_PORTABLE_TAG`], per host-asset marker. Verified 2026-10-01
/// against the release `SHA256SUMS.txt`. Same bump rule as QEMU.
pub const WIMLIB_PORTABLE_SHA256_MACOS_ARM64: &str =
    "913fbbced40f3ce13c3467ba0b9897c974f419f1b5e96cbd623267af7af225b8";
pub const WIMLIB_PORTABLE_SHA256_LINUX_X86_64: &str =
    "64e71d3ef9e00e383b92398afe1b7f641133606fa1bc5226c25fcb4c5af7eaa4";
pub const WIMLIB_PORTABLE_SHA256_LINUX_AARCH64: &str =
    "6f549ce542667ffcbf368da4cde7349b5ae9edaf8f1d76d522d857ddaef1243a";

/// Vendored virtio-win driver pack release tag.
pub const DRIVERS_RELEASE_TAG: &str = "drivers-20260925";

/// SHA-256 of `virtio-win.iso` at [`DRIVERS_RELEASE_TAG`]. Verified
/// 2026-10-01 against the release `virtio-win.iso.sha256` sidecar.
/// BUMP RULE: a new drivers tag means new bytes — update alongside the tag.
/// (The live sidecar is still fetched and must match this pin: a forged
/// sidecar+ISO pair fails the pin check, closing the TOFU gap.)
pub const VIRTIO_WIN_SHA256: &str =
    "303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d";

/// Win32-OpenSSH release tag (note the four-part `10.0.0.0p2` — the
/// three-part `v10.0.0p2` form 404s; tag list at
/// https://github.com/PowerShell/Win32-OpenSSH/releases).
pub const OPENSSH_RELEASE_TAG: &str = "10.0.0.0p2-Preview";

/// SHA-256 of `OpenSSH-Win64.zip` at [`OPENSSH_RELEASE_TAG`], verified
/// 2026-10-01 from the upstream release asset (valid zip, expected layout).
/// BUMP RULE: updating the tag MUST update both hashes.
pub const OPENSSH_SHA256_WIN64: &str =
    "23f50f3458c4c5d0b12217c6a5ddfde0137210a30fa870e98b29827f7b43aba5";

/// SHA-256 of `OpenSSH-ARM64.zip` at [`OPENSSH_RELEASE_TAG`], verified
/// 2026-10-01 from the upstream release asset. Same bump rule as above.
pub const OPENSSH_SHA256_ARM64: &str =
    "698c6aec31c1dd0fb996206e8741f4531a97355686b5431ef347d531b07fcd42";
