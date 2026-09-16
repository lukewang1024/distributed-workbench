//go:build windows

package updater

import (
	"os/exec"
	"strings"
)

func releaseInUse(root string) bool {
	script := "$p=[IO.Path]::GetFullPath($args[0]); Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($p,[StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1 -ExpandProperty ProcessId"
	output, err := exec.Command("powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script, root).Output()
	return err != nil || strings.TrimSpace(string(output)) != ""
}
