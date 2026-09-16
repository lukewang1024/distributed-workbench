//go:build !windows

package updater

import "os"

func replacePath(source, destination string) error { return os.Rename(source, destination) }
