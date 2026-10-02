# Templates: context and further examples

`autounattend.xml` here is a *template* (`{arch}`, `{sshzip}`, …) —
`src/unattend.rs` is the single source of truth. Never hand-edit the copy
staged onto the USB image; change the generator.

## More examples

- dockur/windows unattend collection (broad real-world coverage):
  https://github.com/dockur/windows/tree/master/assets

## Earned constraints (observed live — do not "simplify" these)

- `RunSynchronous` orders must be consecutive from 1, or Setup fails
  with `0x800700D`.
- `&` in XML must be `&amp;`; stripped entities fail with `0x80070001`.
- No `--` inside XML comments (MSXML rejects the whole file).
- No `powershell.exe` in the `windowsPE` pass (absent from WinPE) —
  partition with `diskpart /s`, drivers with `pnputil`, from `cmd.exe`.
- `InstallFrom` is MetaData-only (`Windows 11 Pro`): a stale `Path D:`
  fails with `0x80070490 ELEMENT_NOT_FOUND` (USB letters are
  topology-dependent).
- OpenSSH installs offline from `tools/<zip>` on the USB stick
  (`Expand-Archive` → `install-sshd.ps1` → service Automatic → firewall
  rule), one phase per `RunSynchronousCommand`, logged to
  `C:\ssh-install.log`.
- Target is NVMe (`stornvme` inbox, Disk 0): no `viostor`, no
  `DriverPaths`, no `offlineServicing` in the file.
- Win11 gates bypassed via `HKLM\SYSTEM\Setup\LabConfig`
  (TPM, SecureBoot, RAM, CPU, Storage) + `BypassNRO`.

## Media layout this template assumes

Single-partition FAT32 USB-HDD (`WINSETUP`): full ISO tree minus
`install.wim` + `wimlib-imagex split` SWMs + `drivers/` (NetKVM) +
`tools/<OpenSSH zip>` + this answer file at the root + `setup/part.txt`.
EDK2 boots it with no keypress as `usb-storage,removable=true`.
Built by `build-media`; see `src/media.rs` and `src/commands.rs`.
