//go:build darwin

package updater

import "os/exec"

func releaseInUse(root string) bool {
	command := exec.Command("/usr/sbin/lsof", "-t", "+D", root)
	output, err := command.Output()
	return err == nil && len(output) > 0
}
