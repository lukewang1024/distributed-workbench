//go:build !darwin && !linux && !windows

package updater

func releaseInUse(root string) bool { return true }
