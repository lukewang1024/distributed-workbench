package updater

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

type Paths struct {
	DataRoot   string
	StateRoot  string
	CacheRoot  string
	ConfigRoot string
	BinRoot    string
}

func DefaultPaths() (Paths, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return Paths{}, err
	}
	if runtime.GOOS == "windows" {
		local := os.Getenv("LOCALAPPDATA")
		if local == "" {
			return Paths{}, fmt.Errorf("LOCALAPPDATA is required on Windows")
		}
		return Paths{
			DataRoot: filepath.Join(local, "distributed-workbench-updater", "data"), StateRoot: filepath.Join(local, "distributed-workbench-updater", "state"),
			CacheRoot: filepath.Join(local, "distributed-workbench-updater", "cache"), ConfigRoot: filepath.Join(local, "distributed-workbench-updater", "config"),
			BinRoot: filepath.Join(local, "distributed-workbench-updater", "bin"),
		}, nil
	}
	value := func(name, fallback string) string {
		if current := os.Getenv(name); current != "" {
			return current
		}
		return filepath.Join(home, fallback)
	}
	return Paths{
		DataRoot:   filepath.Join(value("XDG_DATA_HOME", ".local/share"), "distributed-workbench-updater"),
		StateRoot:  filepath.Join(value("XDG_STATE_HOME", ".local/state"), "distributed-workbench-updater"),
		CacheRoot:  filepath.Join(value("XDG_CACHE_HOME", ".cache"), "distributed-workbench-updater", "artifacts"),
		ConfigRoot: filepath.Join(value("XDG_CONFIG_HOME", ".config"), "distributed-workbench"),
		BinRoot:    value("XDG_BIN_HOME", ".local/bin"),
	}, nil
}

func (p Paths) Releases() string     { return filepath.Join(p.DataRoot, "releases") }
func (p Paths) Current() string      { return filepath.Join(p.DataRoot, "current") }
func (p Paths) Shared() string       { return filepath.Join(p.DataRoot, "shared") }
func (p Paths) Lock() string         { return filepath.Join(p.StateRoot, "transaction.lock") }
func (p Paths) Transactions() string { return filepath.Join(p.StateRoot, "transactions") }
func (p Paths) Verified() string     { return filepath.Join(p.StateRoot, "verified") }
func (p Paths) RolloutLock() string  { return filepath.Join(p.StateRoot, "rollout.lock") }
