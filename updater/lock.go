package updater

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

type Lock struct {
	PID         int       `json:"pid"`
	Operation   string    `json:"operation"`
	Version     string    `json:"version"`
	Transaction string    `json:"transactionId"`
	StartedAt   time.Time `json:"startedAt"`
	path        string
}

func AcquireLock(path, operation, version string) (*Lock, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}
	lock := &Lock{PID: os.Getpid(), Operation: operation, Version: version, Transaction: fmt.Sprintf("%d-%d", time.Now().UTC().UnixNano(), os.Getpid()), StartedAt: time.Now().UTC(), path: path}
	data, _ := json.MarshalIndent(lock, "", "  ")
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if os.IsExist(err) {
		existing, readErr := os.ReadFile(path)
		if readErr == nil {
			return nil, fmt.Errorf("another transaction holds the lock: %s", existing)
		}
		return nil, fmt.Errorf("another transaction holds %s", path)
	}
	if err != nil {
		return nil, err
	}
	if _, err = file.Write(append(data, '\n')); err != nil {
		file.Close()
		os.Remove(path)
		return nil, err
	}
	if err = file.Close(); err != nil {
		os.Remove(path)
		return nil, err
	}
	return lock, nil
}

func (l *Lock) Release() error { return os.Remove(l.path) }

func UnlockStale(path string) error {
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	var lock Lock
	if err := json.Unmarshal(data, &lock); err != nil {
		return fmt.Errorf("cannot parse lock; refusing automatic removal: %w", err)
	}
	if processAlive(lock.PID) {
		return fmt.Errorf("lock owner pid %d is still running", lock.PID)
	}
	return os.Remove(path)
}
