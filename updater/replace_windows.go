//go:build windows

package updater

import "os"

func replacePath(source, destination string) error {
	backup := destination + ".previous"
	_ = os.Remove(backup)
	if _, err := os.Stat(destination); err == nil {
		if err = os.Rename(destination, backup); err != nil {
			return err
		}
	}
	if err := os.Rename(source, destination); err != nil {
		_ = os.Rename(backup, destination)
		return err
	}
	_ = os.Remove(backup)
	return nil
}
