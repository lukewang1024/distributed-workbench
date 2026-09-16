//go:build linux

package updater

import (
	"os"
	"path/filepath"
	"strings"
)

func releaseInUse(root string) bool {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return true
	}
	root = filepath.Clean(root) + string(filepath.Separator)
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		for _, name := range []string{"exe", "cwd"} {
			target, err := os.Readlink(filepath.Join("/proc", entry.Name(), name))
			if err == nil && strings.HasPrefix(filepath.Clean(target)+string(filepath.Separator), root) {
				return true
			}
		}
	}
	return false
}
