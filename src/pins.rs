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
/// [`RELEASE_REPO`] (<https://github.com/jzombie/lab-qemu-portable>) —
/// never an upstream version number. Rebuilds take `qemu-vX.Y.Z-<run>`
/// while the proven tag stays intact, so the tag is the only unambiguous
/// build identity (binaries, bundled libs, firmware as one unit).
/// `run`/`install` use exactly this; `fetch-qemu` downloads exactly this
/// unless `--qemu-tag` overrides for new-build testing.
pub const QEMU_PORTABLE_TAG: &str = "qemu-v11.1.3";

/// SHA-256 of the pinned QEMU portable archives at [`QEMU_PORTABLE_TAG`],
/// per host-asset marker (see `MARKER_*` in [`crate::paths`]). Verified
/// 2026-11-10 against the release `SHA256SUMS.txt`.
/// BUMP RULE: a new tag means new bytes — update all three alongside the tag.
pub const QEMU_PORTABLE_SHA256_MACOS_ARM64: &str =
    "a67a44e662105811ea4e9ecd9b5f8997420dcae6168860a12f8a8ca97372bf36";
pub const QEMU_PORTABLE_SHA256_LINUX_X86_64: &str =
    "01c72e10bc05800fa21573347b0e4edd2f4bcc6fce0d071ddc303ccfd0a19e88";
pub const QEMU_PORTABLE_SHA256_LINUX_AARCH64: &str =
    "1b82edd1e4ac083ac363b8e28d27d631a2fd9902298df82c22735266031f1721";

/// Pinned portable wimlib release, as a full release tag in
/// [`RELEASE_REPO`] — never a bare version. Same contract as QEMU:
/// rebuilds take suffixed tags, so the tag is the only unambiguous
/// build identity. `build-media` uses exactly this; `fetch-wimlib`
/// downloads exactly this unless `--wimlib-tag` overrides.
pub const WIMLIB_PORTABLE_TAG: &str = "wimlib-v1.14.5-50";

/// SHA-256 of the pinned wimlib portable archives at
/// [`WIMLIB_PORTABLE_TAG`], per host-asset marker. Verified 2026-11-10
/// against the release `SHA256SUMS.txt`. Same bump rule as QEMU.
pub const WIMLIB_PORTABLE_SHA256_MACOS_ARM64: &str =
    "2f73107dad9c2b63348fe5856488be236a4f483c0dd618ef5cf26ca9491c8686";
pub const WIMLIB_PORTABLE_SHA256_LINUX_X86_64: &str =
    "1f1497f6e8ae5f35c2cae247d496ab7985e64f6ff7bc3a7c5bb589a2d5f53360";
pub const WIMLIB_PORTABLE_SHA256_LINUX_AARCH64: &str =
    "8fd1c8de44950be2476f1d196008f4fe132fc5a66ca27b8ce1ac37c2c463df68";

/// Vendored virtio-win driver pack release tag.
pub const DRIVERS_RELEASE_TAG: &str = "drivers-20261109";

/// SHA-256 of `virtio-win.iso` at [`DRIVERS_RELEASE_TAG`]. Verified
/// 2026-10-12 against the release `virtio-win.iso.sha256` sidecar
/// (identical bytes to the previous pack — tag-only bump).
/// BUMP RULE: a new drivers tag means new bytes — update alongside the tag.
/// (The live sidecar is still fetched and must match this pin: a forged
/// sidecar+ISO pair fails the pin check, closing the TOFU gap.)
pub const VIRTIO_WIN_SHA256: &str =
    "303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d";

/// Win32-OpenSSH release tag (note the four-part `10.0.0.0p2` — the
/// three-part `v10.0.0p2` form 404s; tag list at
/// <https://github.com/PowerShell/Win32-OpenSSH/releases>).
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
