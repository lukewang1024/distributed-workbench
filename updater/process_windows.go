//go:build windows

package updater

import (
	"os/exec"
	"strconv"
	"strings"
)

func processAlive(pid int) bool {
	out, err := exec.Command("tasklist", "/FI", "PID eq "+strconv.Itoa(pid), "/FO", "CSV", "/NH").Output()
	return err == nil && strings.Contains(string(out), strconv.Itoa(pid))
}
