# Guest readiness probe (runs over SSH after the first banner).
# Prints READY when Setup is truly finished, else BUSY:<why>.
# Victory conditions: SetupType 0, no OOBE in progress, no CBS reboot
# pending, no Windows Update reboot required.
$s = Get-ItemProperty 'HKLM:\SYSTEM\Setup'
$r1 = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending'
$r2 = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired'
if ($s.SetupType -eq 0 -and -not $s.OOBEInProgress -and -not $r1 -and -not $r2) {
  'READY'
} else {
  "BUSY setuptype=$($s.SetupType) oobe=$($s.OOBEInProgress) cbs=$r1 wu=$r2"
}
