# OOBE Bypass Runbook (canary-red → bypass-green)

When the weekly canary fails with a framebuffer screenshot of a new blocking
screen, follow this loop. Modern OOBE pages are CloudExperienceHost web apps
that gate on registry state.

## 1. Repro locally with a display
Relaunch the exact failing QEMU cmdline with `-display gtk` (Linux) or
`-display cocoa` (macOS) instead of `-display none` (or use
`-display vnc=127.0.0.1:0` + a VNC client).

## 2. Shift+F10 registry diff (SYSTEM context in WinPE/early Setup)
1. At the blocking screen press **Shift+F10** (hidden `cmd.exe`).
2. `reg export HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE C:\before.reg`
   (also export `HKCU\...\CloudExperienceHost` on late OOBE screens where
   Shift+F10 is user-context). Do **not** write to the vvfat drive — it is
   `ro`-frozen at launch. Write to `C:\`, then `scp` out over SSH.
3. Click through the screen once manually.
4. At the next screen, Shift+F10 again → `C:\after.reg`.
5. Diff. A new `REG_DWORD` (e.g. `HideNewX=1`) is the bypass — add it as a
   `RunSynchronousCommand` reg entry in the `specialize` pass.

## 3. Inspect CloudExperienceHost sources
`C:\Windows\SystemApps\Microsoft.Windows.CloudExperienceHost*` contains the
per-page JS/JSON. Search for the registry/policy key each page checks to
decide render-vs-skip.

## 4. Check the ADK Unattend schema
Microsoft Learn → Windows ADK Unattend reference lists valid `<OOBE>` children
per release. A new documented tag goes in the `oobeSystem` pass.

## 5. Track the big three (usually patched ≤48h after Insider builds)
- Rufus (`rufus.c` bypass section)
- HashiCorp Packer Windows community templates (`autounattend.xml`)
- `dockur/windows` (Docker QEMU wrapper)

Each new bypass lands in this crate as one `RunSynchronous` reg entry, then
re-run the canary.
