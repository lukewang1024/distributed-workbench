package updater

import (
	"archive/tar"
	"archive/zip"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"time"
)

type Policy struct {
	PackageID            string
	AllowLegacyIdentity  bool
	ManifestSchema       string
	RecommendationSchema string
	RolloutSchema        string
	RequiredPlatforms    []string
	Headers              func(*url.URL) http.Header
}

type Updater struct {
	Policy Policy

	Paths               Paths
	HTTPClient          *http.Client
	Stdout              io.Writer
	Stderr              io.Writer
	Version             string
	FabricCheckTimeout  time.Duration
	FabricCheckInterval time.Duration
	downloadRetryDelay  time.Duration
}

const (
	releaseDownloadTimeout = 30 * time.Minute
	downloadMaxAttempts    = 12
	downloadMaxRetryDelay  = 4 * time.Second
	artifactCacheRetention = 24 * time.Hour
)

type transaction struct {
	ID             string    `json:"id"`
	Operation      string    `json:"operation"`
	TargetVersion  string    `json:"targetVersion"`
	Previous       string    `json:"previousVersion,omitempty"`
	ManifestDigest string    `json:"manifestDigest,omitempty"`
	Stage          string    `json:"stage"`
	Status         string    `json:"status"`
	StartedAt      time.Time `json:"startedAt"`
	FinishedAt     time.Time `json:"finishedAt,omitempty"`
	Error          string    `json:"error,omitempty"`
	Actor          string    `json:"actor,omitempty"`
	OverrideReason string    `json:"overrideReason,omitempty"`
}

type releaseRecord struct {
	PackageID      string            `json:"packageId"`
	Version        string            `json:"version"`
	ManifestDigest string            `json:"manifestDigest"`
	StateFormat    int               `json:"stateFormat"`
	Status         string            `json:"status"`
	InstalledAt    time.Time         `json:"installedAt"`
	VerifiedAt     *time.Time        `json:"verifiedAt,omitempty"`
	Files          map[string]string `json:"files,omitempty"`
}

type RecommendedIndex struct {
	SchemaVersion    string `json:"schemaVersion"`
	UpdaterVersion   string `json:"updaterVersion"`
	WorkbenchVersion string `json:"workbenchVersion"`
	ManifestURL      string `json:"manifestUrl"`
	ManifestSHA256   string `json:"manifestSha256"`
}

type Status struct {
	PackageID       string          `json:"packageId"`
	Desired         string          `json:"desired,omitempty"`
	Current         string          `json:"current,omitempty"`
	CurrentRecord   *releaseSummary `json:"currentRecord,omitempty"`
	Installed       []string        `json:"installed"`
	Lock            json.RawMessage `json:"lock,omitempty"`
	Integrity       string          `json:"integrity"`
	LastTransaction *transaction    `json:"lastTransaction,omitempty"`
	Rollout         *RolloutState   `json:"rollout,omitempty"`
}

type releaseSummary struct {
	Version        string     `json:"version"`
	ManifestDigest string     `json:"manifestDigest"`
	StateFormat    int        `json:"stateFormat"`
	Status         string     `json:"status"`
	InstalledAt    time.Time  `json:"installedAt"`
	VerifiedAt     *time.Time `json:"verifiedAt,omitempty"`
}

func New(paths Paths) *Updater {
	return &Updater{Policy: Policy{PackageID: "distributed-workbench", ManifestSchema: ManifestSchema, RecommendationSchema: "distributed-workbench.recommended/v1", RolloutSchema: "distributed-workbench.rollout/v1"}, Paths: paths, HTTPClient: &http.Client{Timeout: releaseDownloadTimeout}, Stdout: os.Stdout, Stderr: os.Stderr, Version: "dev", FabricCheckTimeout: 90 * time.Second, FabricCheckInterval: 2 * time.Second, downloadRetryDelay: 250 * time.Millisecond}
}

func Platform() (string, error) {
	value := runtime.GOOS + "-" + runtime.GOARCH
	if value != "darwin-arm64" && value != "linux-amd64" && value != "windows-amd64" {
		return "", fmt.Errorf("unsupported platform %s", value)
	}
	return value, nil
}

func (u *Updater) Bootstrap(ctx context.Context, indexLocation, expectedDigest string) error {
	return u.bootstrap(ctx, indexLocation, expectedDigest, indexLocation)
}

// BootstrapWithBase verifies a locally fetched recommendation while resolving
// its relative manifest URL against the remote recommendation location.
func (u *Updater) BootstrapWithBase(ctx context.Context, indexLocation, expectedDigest, referenceBase string) error {
	return u.bootstrap(ctx, indexLocation, expectedDigest, referenceBase)
}

// Reconcile pulls a mutable desired-release index together with its digest
// sidecar, then enters the same exact-version installation path as bootstrap.
// Nodes call this themselves, so an offline node does not require the rollout
// coordinator to open an inbound SSH connection when it returns.
func (u *Updater) Reconcile(ctx context.Context, indexLocation, digestLocation string) error {
	digestData, err := u.fetchBytes(ctx, digestLocation)
	if err != nil {
		return fmt.Errorf("fetch recommendation digest: %w", err)
	}
	expected := strings.Fields(string(digestData))
	if len(expected) != 1 || !validSHA256(strings.ToLower(expected[0])) {
		return fmt.Errorf("invalid recommendation digest sidecar")
	}
	return u.bootstrap(ctx, indexLocation, strings.ToLower(expected[0]), indexLocation)
}

func (u *Updater) bootstrap(ctx context.Context, indexLocation, expectedDigest, referenceBase string) error {
	data, err := u.fetchBytes(ctx, indexLocation)
	if err != nil {
		return err
	}
	if expectedDigest == "" || digestBytes(data) != strings.ToLower(expectedDigest) {
		return fmt.Errorf("recommended index SHA-256 mismatch or missing expected digest")
	}
	var index RecommendedIndex
	decoder := json.NewDecoder(strings.NewReader(string(data)))
	decoder.DisallowUnknownFields()
	if err = decoder.Decode(&index); err != nil {
		return fmt.Errorf("parse recommended index: %w", err)
	}
	if index.SchemaVersion != u.Policy.RecommendationSchema || !exactVersion.MatchString(index.WorkbenchVersion) || !validSHA256(index.ManifestSHA256) || index.ManifestURL == "" {
		return fmt.Errorf("invalid recommended index")
	}
	index.ManifestURL, err = resolveReference(referenceBase, index.ManifestURL)
	if err != nil {
		return fmt.Errorf("resolve manifest URL: %w", err)
	}
	if u.Version != "dev" && index.UpdaterVersion != u.Version {
		return fmt.Errorf("recommended index expects updater %s; running %s", index.UpdaterVersion, u.Version)
	}
	if current, currentErr := u.CurrentVersion(); currentErr == nil && current == index.WorkbenchVersion {
		record, recordErr := u.loadVerifiedRecord(current)
		if recordErr != nil {
			return u.repairCurrentVerifiedRecord(ctx, current, index.ManifestSHA256)
		}
		if record.ManifestDigest != strings.ToLower(index.ManifestSHA256) {
			return fmt.Errorf("current release %s manifest digest does not match desired state", current)
		}
		_, _ = u.pruneArtifactCache(time.Now().UTC())
		return nil
	}
	return u.Install(ctx, index.WorkbenchVersion, index.ManifestURL, index.ManifestSHA256, false, false, "")
}

func (u *Updater) repairCurrentVerifiedRecord(ctx context.Context, version, expectedManifestDigest string) error {
	root := filepath.Join(u.Paths.Releases(), version)
	manifestPath := filepath.Join(root, ".manifest.json")
	manifestBytes, err := os.ReadFile(manifestPath)
	if err != nil {
		return fmt.Errorf("current release %s has no verified record and manifest is unavailable: %w", version, err)
	}
	manifestDigest := digestBytes(manifestBytes)
	if manifestDigest != strings.ToLower(expectedManifestDigest) {
		return fmt.Errorf("current release %s manifest digest does not match desired state", version)
	}
	manifest, err := u.parseManifest(manifestBytes)
	if err != nil {
		return fmt.Errorf("current release %s manifest is invalid: %w", version, err)
	}
	platform, err := Platform()
	if err != nil {
		return err
	}
	installedFiles, err := installedFileDigests(root, manifest, platform)
	if err != nil {
		return err
	}
	if err = verifyFiles(root, installedFiles); err != nil {
		return fmt.Errorf("current release %s failed integrity verification: %w", version, err)
	}
	healthCheck := manifest.HealthCheck
	if manifest.Artifacts[platform].HealthCheck != nil {
		healthCheck = manifest.Artifacts[platform].HealthCheck
	}
	if healthCheck != nil {
		if err = u.healthCheck(ctx, root, healthCheck); err != nil {
			return fmt.Errorf("current release %s failed health check: %w", version, err)
		}
	}
	now := time.Now().UTC()
	record := releaseRecord{PackageID: u.Policy.PackageID,
		Version:        version,
		ManifestDigest: manifestDigest,
		StateFormat:    manifest.StateFormat,
		Status:         "verified",
		InstalledAt:    now,
		VerifiedAt:     &now,
		Files:          installedFiles,
	}
	data, _ := json.MarshalIndent(record, "", "  ")
	releaseRecordPath := filepath.Join(root, ".release.json")
	if err = os.Chmod(releaseRecordPath, 0o644); err == nil {
		err = os.WriteFile(releaseRecordPath, append(data, '\n'), 0o644)
	}
	if err == nil {
		err = os.Chmod(releaseRecordPath, 0o444)
	}
	if err != nil {
		return err
	}
	if err = u.writeVerifiedRecord(record); err != nil {
		return err
	}
	_, _ = u.pruneArtifactCache(time.Now().UTC())
	return nil
}

func (u *Updater) Install(ctx context.Context, version, manifestLocation, expectedManifestDigest string, allowDeprecated, forceRevoked bool, reason string) error {
	if !exactVersion.MatchString(version) {
		return fmt.Errorf("install requires an exact version such as v1.2.3")
	}
	lock, err := AcquireLock(u.Paths.Lock(), "install", version)
	if err != nil {
		return err
	}
	defer lock.Release()
	tx := &transaction{ID: lock.Transaction, Operation: "install", TargetVersion: version, Stage: "manifest", Status: "running", StartedAt: time.Now().UTC(), Actor: os.Getenv("USER"), OverrideReason: reason}
	defer func() { _ = u.writeTransaction(tx) }()
	manifestBytes, err := u.fetchBytes(ctx, manifestLocation)
	if err != nil {
		return u.fail(tx, err)
	}
	manifestDigest := digestBytes(manifestBytes)
	if expectedManifestDigest != "" && manifestDigest != strings.ToLower(expectedManifestDigest) {
		return u.fail(tx, fmt.Errorf("manifest SHA-256 mismatch: got %s", manifestDigest))
	}
	manifest, err := u.parseManifest(manifestBytes)
	if err != nil {
		return u.fail(tx, err)
	}
	if err = resolveManifestReferences(manifestLocation, manifest); err != nil {
		return u.fail(tx, err)
	}
	if manifest.Version != version {
		return u.fail(tx, fmt.Errorf("manifest version %s does not match requested %s", manifest.Version, version))
	}
	if u.Version != "dev" && manifest.MinimumUpdater != "" && compareVersion(u.Version, manifest.MinimumUpdater) < 0 {
		return u.fail(tx, fmt.Errorf("release requires updater %s or newer; running %s", manifest.MinimumUpdater, u.Version))
	}
	if manifest.Status == "revoked" && !forceRevoked {
		return u.fail(tx, fmt.Errorf("release %s is revoked; use --force-revoked only for audited recovery", version))
	}
	if manifest.Status == "revoked" && strings.TrimSpace(reason) == "" {
		return u.fail(tx, fmt.Errorf("--force-revoked requires --reason for the audit log"))
	}
	if manifest.Status == "deprecated" && !allowDeprecated {
		return u.fail(tx, fmt.Errorf("release %s is deprecated; use --allow-deprecated after review", version))
	}
	platform, err := Platform()
	if err != nil {
		return u.fail(tx, err)
	}
	if err = u.prepare(); err != nil {
		return u.fail(tx, err)
	}
	tx.ManifestDigest = manifestDigest
	tx.Previous, _ = u.CurrentVersion()
	if tx.Previous != "" {
		if current, recordErr := u.loadVerifiedRecord(tx.Previous); recordErr == nil && current.StateFormat != manifest.StateFormat {
			return u.fail(tx, fmt.Errorf("automatic upgrade requires backward-compatible state format %d; release declares %d", current.StateFormat, manifest.StateFormat))
		}
	}
	root := filepath.Join(u.Paths.Releases(), version)
	if _, err = os.Stat(root); err == nil {
		record, recordErr := u.loadVerifiedRecord(version)
		if recordErr != nil {
			return u.fail(tx, fmt.Errorf("installed release %s is not verified: %w", version, recordErr))
		}
		if record.ManifestDigest != manifestDigest {
			return u.fail(tx, fmt.Errorf(
				"installed release %s manifest digest %s does not match requested %s",
				version, record.ManifestDigest, manifestDigest,
			))
		}
		if err = verifyFiles(root, record.Files); err != nil {
			return u.fail(tx, fmt.Errorf("installed release %s failed integrity verification: %w", version, err))
		}
		tx.Stage = "activate-existing"
		if err = u.setCurrent(version); err != nil {
			return u.fail(tx, err)
		}
		healthCheck := manifest.HealthCheck
		if manifest.Artifacts[platform].HealthCheck != nil {
			healthCheck = manifest.Artifacts[platform].HealthCheck
		}
		if healthCheck != nil {
			if err = u.healthCheck(ctx, root, healthCheck); err != nil {
				_ = u.restoreCurrent(tx.Previous)
				return u.fail(tx, fmt.Errorf("existing release failed health check; restored %s: %w", tx.Previous, err))
			}
		}
		tx.Stage, tx.Status, tx.FinishedAt = "complete", "ok", time.Now().UTC()
		if err = u.writeTransaction(tx); err != nil {
			return err
		}
		_, _ = u.pruneArtifactCache(time.Now().UTC())
		return nil
	} else if !os.IsNotExist(err) {
		return u.fail(tx, err)
	}
	tx.Stage = "download"
	mainArchive, err := u.downloadArtifact(ctx, manifest.Artifacts[platform])
	if err != nil {
		return u.fail(tx, err)
	}
	dependencyArchives := make([]string, len(manifest.Dependencies))
	for index, dependency := range manifest.Dependencies {
		artifact, ok := dependency.Artifacts[platform]
		if !ok {
			continue
		}
		dependencyArchives[index], err = u.downloadArtifact(ctx, artifact)
		if err != nil {
			return u.fail(tx, fmt.Errorf("dependency %s: %w", dependency.Name, err))
		}
	}
	staging, err := os.MkdirTemp(u.Paths.Releases(), ".staging-"+version+"-")
	if err != nil {
		return u.fail(tx, err)
	}
	committed := false
	defer func() {
		if !committed {
			_ = os.RemoveAll(staging)
		}
	}()
	tx.Stage = "extract"
	if err = extractArchive(mainArchive, manifest.Artifacts[platform].Format, manifest.Artifacts[platform].StripComponents, staging); err != nil {
		return u.fail(tx, err)
	}
	for index, dependency := range manifest.Dependencies {
		artifact, ok := dependency.Artifacts[platform]
		if !ok {
			continue
		}
		destination := filepath.Join(staging, "dependencies", dependency.Name)
		if err = os.MkdirAll(destination, 0o755); err != nil {
			return u.fail(tx, err)
		}
		if err = extractArchive(dependencyArchives[index], artifact.Format, artifact.StripComponents, destination); err != nil {
			return u.fail(tx, err)
		}
		metadata, marshalErr := json.MarshalIndent(map[string]string{
			"repository": dependency.Repository,
			"revision":   dependency.Revision,
			"version":    dependency.Version,
		}, "", "  ")
		if marshalErr != nil {
			return u.fail(tx, marshalErr)
		}
		if err = os.WriteFile(filepath.Join(destination, ".workbench-artifact.json"), append(metadata, '\n'), 0o444); err != nil {
			return u.fail(tx, err)
		}
	}
	installedFiles := map[string]string{}
	for path, digest := range manifest.Artifacts[platform].Files {
		installedFiles[path] = digest
	}
	if err = verifyFiles(staging, manifest.Artifacts[platform].Files); err != nil {
		return u.fail(tx, err)
	}
	for _, dependency := range manifest.Dependencies {
		artifact, ok := dependency.Artifacts[platform]
		if !ok {
			continue
		}
		dependencyRoot := filepath.Join(staging, "dependencies", dependency.Name)
		if err = verifyFiles(dependencyRoot, artifact.Files); err != nil {
			return u.fail(tx, fmt.Errorf("dependency %s: %w", dependency.Name, err))
		}
		for path, digest := range artifact.Files {
			installedFiles[filepath.ToSlash(filepath.Join("dependencies", dependency.Name, path))] = digest
		}
		metadataPath := filepath.Join(dependencyRoot, ".workbench-artifact.json")
		metadataDigest, digestErr := digestFile(metadataPath)
		if digestErr != nil {
			return u.fail(tx, digestErr)
		}
		installedFiles[filepath.ToSlash(filepath.Join("dependencies", dependency.Name, ".workbench-artifact.json"))] = metadataDigest
	}
	for path, digest := range manifest.Files {
		installedFiles[path] = digest
	}
	if err = verifyFiles(staging, manifest.Files); err != nil {
		return u.fail(tx, err)
	}
	record := releaseRecord{PackageID: u.Policy.PackageID, Version: version, ManifestDigest: manifestDigest, StateFormat: manifest.StateFormat, Status: manifest.Status, InstalledAt: time.Now().UTC(), Files: installedFiles}
	data, _ := json.MarshalIndent(record, "", "  ")
	if err = os.WriteFile(filepath.Join(staging, ".release.json"), append(data, '\n'), 0o444); err != nil {
		return u.fail(tx, err)
	}
	manifestCopy := filepath.Join(staging, ".manifest.json")
	if err = os.WriteFile(manifestCopy, manifestBytes, 0o444); err != nil {
		return u.fail(tx, err)
	}
	final := filepath.Join(u.Paths.Releases(), version)
	if err = os.Rename(staging, final); err != nil {
		return u.fail(tx, err)
	}
	committed = true
	tx.Stage = "switch"
	if err = u.setCurrent(version); err != nil {
		_ = os.RemoveAll(final)
		return u.fail(tx, err)
	}
	commands := manifest.Commands
	if len(manifest.Artifacts[platform].Commands) > 0 {
		commands = manifest.Artifacts[platform].Commands
	}
	if err = u.installLaunchers(commands); err != nil {
		recoveryErr := u.restoreAndVerify(ctx, tx.Previous)
		_ = os.RemoveAll(final)
		if recoveryErr != nil {
			return u.fail(tx, fmt.Errorf("launcher install failed: %v; previous release recovery failed: %w", err, recoveryErr))
		}
		return u.fail(tx, err)
	}
	healthCheck := manifest.HealthCheck
	if manifest.Artifacts[platform].HealthCheck != nil {
		healthCheck = manifest.Artifacts[platform].HealthCheck
	}
	if healthCheck != nil {
		tx.Stage = "health-check"
		if err = u.healthCheck(ctx, final, healthCheck); err != nil {
			recoveryErr := u.restoreAndVerify(ctx, tx.Previous)
			_ = os.RemoveAll(final)
			if recoveryErr != nil {
				return u.fail(tx, fmt.Errorf("new release health check failed: %v; previous release recovery failed: %w", err, recoveryErr))
			}
			return u.fail(tx, fmt.Errorf("health check failed and current was rolled back to %s: %w", tx.Previous, err))
		}
	}
	now := time.Now().UTC()
	record.VerifiedAt = &now
	record.Status = "verified"
	data, _ = json.MarshalIndent(record, "", "  ")
	if err = os.Chmod(filepath.Join(final, ".release.json"), 0o644); err == nil {
		err = os.WriteFile(filepath.Join(final, ".release.json"), append(data, '\n'), 0o644)
	}
	if err == nil {
		err = os.Chmod(filepath.Join(final, ".release.json"), 0o444)
	}
	if err == nil {
		err = u.writeVerifiedRecord(record)
	}
	if err != nil {
		recoveryErr := u.restoreAndVerify(ctx, tx.Previous)
		_ = os.RemoveAll(final)
		if recoveryErr != nil {
			return u.fail(tx, fmt.Errorf("release verification state failed: %v; previous release recovery failed: %w", err, recoveryErr))
		}
		return u.fail(tx, err)
	}
	tx.Stage, tx.Status, tx.FinishedAt = "complete", "ok", time.Now().UTC()
	if err = u.writeTransaction(tx); err != nil {
		return err
	}
	_, _ = u.pruneArtifactCache(time.Now().UTC())
	return nil
}

func installedFileDigests(root string, manifest *Manifest, platform string) (map[string]string, error) {
	installedFiles := map[string]string{}
	for path, digest := range manifest.Artifacts[platform].Files {
		installedFiles[path] = digest
	}
	for _, dependency := range manifest.Dependencies {
		artifact, ok := dependency.Artifacts[platform]
		if !ok {
			continue
		}
		for path, digest := range artifact.Files {
			installedFiles[filepath.ToSlash(filepath.Join("dependencies", dependency.Name, path))] = digest
		}
		metadataPath := filepath.Join(root, "dependencies", dependency.Name, ".workbench-artifact.json")
		metadataDigest, err := digestFile(metadataPath)
		if err != nil {
			return nil, err
		}
		installedFiles[filepath.ToSlash(filepath.Join("dependencies", dependency.Name, ".workbench-artifact.json"))] = metadataDigest
	}
	for path, digest := range manifest.Files {
		installedFiles[path] = digest
	}
	return installedFiles, nil
}

func (u *Updater) Rollback(ctx context.Context, version string, forceRevoked bool, reason string) error {
	if !exactVersion.MatchString(version) {
		return fmt.Errorf("rollback requires an exact installed version")
	}
	lock, err := AcquireLock(u.Paths.Lock(), "rollback", version)
	if err != nil {
		return err
	}
	defer lock.Release()
	tx := &transaction{ID: lock.Transaction, Operation: "rollback", TargetVersion: version, Stage: "validate", Status: "running", StartedAt: time.Now().UTC(), Actor: os.Getenv("USER"), OverrideReason: reason}
	defer func() { _ = u.writeTransaction(tx) }()
	root := filepath.Join(u.Paths.Releases(), version)
	manifestBytes, err := os.ReadFile(filepath.Join(root, ".manifest.json"))
	if err != nil {
		return u.fail(tx, err)
	}
	manifest, err := u.parseManifest(manifestBytes)
	if err != nil {
		return u.fail(tx, err)
	}
	if manifest.Status == "revoked" && !forceRevoked {
		return u.fail(tx, fmt.Errorf("release is revoked"))
	}
	if manifest.Status == "revoked" && strings.TrimSpace(reason) == "" {
		return u.fail(tx, fmt.Errorf("--force-revoked requires --reason for the audit log"))
	}
	record, err := u.loadVerifiedRecord(version)
	if err != nil {
		return u.fail(tx, err)
	}
	if err = verifyFiles(root, record.Files); err != nil {
		return u.fail(tx, err)
	}
	tx.Previous, _ = u.CurrentVersion()
	if err = u.setCurrent(version); err != nil {
		return u.fail(tx, err)
	}
	platform, platformErr := Platform()
	if platformErr != nil {
		return u.fail(tx, platformErr)
	}
	healthCheck := manifest.HealthCheck
	if manifest.Artifacts[platform].HealthCheck != nil {
		healthCheck = manifest.Artifacts[platform].HealthCheck
	}
	if healthCheck != nil {
		if err = u.healthCheck(ctx, root, healthCheck); err != nil {
			_ = u.restoreCurrent(tx.Previous)
			return u.fail(tx, fmt.Errorf("rollback target failed health check; restored %s: %w", tx.Previous, err))
		}
	}
	tx.Stage, tx.Status, tx.FinishedAt = "complete", "ok", time.Now().UTC()
	return u.writeTransaction(tx)
}

func (u *Updater) Verify(version string) error {
	if version == "" {
		version, _ = u.CurrentVersion()
	}
	if version == "" {
		return fmt.Errorf("no current release")
	}
	root := filepath.Join(u.Paths.Releases(), version)
	data, err := os.ReadFile(filepath.Join(root, ".manifest.json"))
	if err != nil {
		return err
	}
	if _, err := u.parseManifest(data); err != nil {
		return err
	}
	record, err := u.loadVerifiedRecord(version)
	if err != nil {
		return err
	}
	manifestDigest, err := digestFile(filepath.Join(root, ".manifest.json"))
	if err != nil {
		return err
	}
	if manifestDigest != record.ManifestDigest {
		return fmt.Errorf("installed manifest digest does not match verified state")
	}
	return verifyFiles(root, record.Files)
}

func (u *Updater) CurrentVersion() (string, error) {
	if runtime.GOOS == "windows" {
		data, err := os.ReadFile(u.Paths.Current())
		if err != nil {
			return "", err
		}
		return strings.TrimSpace(string(data)), nil
	}
	target, err := os.Readlink(u.Paths.Current())
	if err != nil {
		return "", err
	}
	return filepath.Base(target), nil
}

func (u *Updater) setCurrent(version string) error {
	if !exactVersion.MatchString(version) {
		return fmt.Errorf("invalid current version %q", version)
	}
	if _, err := os.Stat(filepath.Join(u.Paths.Releases(), version)); err != nil {
		return err
	}
	if err := os.MkdirAll(u.Paths.DataRoot, 0o755); err != nil {
		return err
	}
	temporary := u.Paths.Current() + fmt.Sprintf(".tmp-%d", os.Getpid())
	_ = os.Remove(temporary)
	if runtime.GOOS == "windows" {
		if err := os.WriteFile(temporary, []byte(version+"\n"), 0o644); err != nil {
			return err
		}
	} else if err := os.Symlink(filepath.Join("releases", version), temporary); err != nil {
		return err
	}
	if err := replacePath(temporary, u.Paths.Current()); err != nil {
		_ = os.Remove(temporary)
		return err
	}
	return nil
}

func (u *Updater) restoreCurrent(version string) error {
	if version == "" {
		return os.Remove(u.Paths.Current())
	}
	return u.setCurrent(version)
}

func (u *Updater) restoreAndVerify(ctx context.Context, version string) error {
	if err := u.restoreCurrent(version); err != nil {
		return err
	}
	if version == "" {
		return nil
	}
	root := filepath.Join(u.Paths.Releases(), version)
	data, err := os.ReadFile(filepath.Join(root, ".manifest.json"))
	if err != nil {
		return err
	}
	manifest, err := u.parseManifest(data)
	if err != nil {
		return err
	}
	platform, err := Platform()
	if err != nil {
		return err
	}
	check := manifest.HealthCheck
	if manifest.Artifacts[platform].HealthCheck != nil {
		check = manifest.Artifacts[platform].HealthCheck
	}
	if check == nil {
		return nil
	}
	var last error
	for attempt := 0; attempt < 2; attempt++ {
		if last = u.healthCheck(ctx, root, check); last == nil {
			return nil
		}
	}
	return last
}

func (u *Updater) ResolveCommand(name string) (string, error) {
	version, err := u.CurrentVersion()
	if err != nil {
		return "", err
	}
	data, err := os.ReadFile(filepath.Join(u.Paths.Releases(), version, ".manifest.json"))
	if err != nil {
		return "", err
	}
	manifest, err := u.parseManifest(data)
	if err != nil {
		return "", err
	}
	commands := manifest.Commands
	if platform, platformErr := Platform(); platformErr == nil && len(manifest.Artifacts[platform].Commands) > 0 {
		commands = manifest.Artifacts[platform].Commands
	}
	relative, ok := commands[name]
	if !ok {
		return "", fmt.Errorf("release %s does not provide command %s", version, name)
	}
	path := filepath.Join(u.Paths.Releases(), version, filepath.FromSlash(relative))
	if _, err := os.Stat(path); err != nil {
		return "", err
	}
	return path, nil
}

func (u *Updater) Exec(name string, args []string, stdin io.Reader, stdout, stderr io.Writer) error {
	path, err := u.ResolveCommand(name)
	if err != nil {
		return err
	}
	var command *exec.Cmd
	if runtime.GOOS == "windows" && strings.EqualFold(filepath.Ext(path), ".cmd") {
		command = exec.Command("cmd.exe", append([]string{"/d", "/s", "/c", path}, args...)...)
	} else {
		command = exec.Command(path, args...)
	}
	command.Stdin = stdin
	command.Stdout = stdout
	command.Stderr = stderr
	return command.Run()
}

func (u *Updater) Status(verify bool) (*Status, error) {
	status := &Status{PackageID: u.Policy.PackageID, Integrity: "not-checked"}
	entries, err := os.ReadDir(u.Paths.Releases())
	if err != nil && !os.IsNotExist(err) {
		return nil, err
	}
	for _, entry := range entries {
		if entry.IsDir() && exactVersion.MatchString(entry.Name()) {
			status.Installed = append(status.Installed, entry.Name())
		}
	}
	sort.Strings(status.Installed)
	status.Current, _ = u.CurrentVersion()
	if status.Current != "" {
		if record, err := u.loadVerifiedRecord(status.Current); err == nil {
			status.CurrentRecord = &releaseSummary{Version: record.Version, ManifestDigest: record.ManifestDigest, StateFormat: record.StateFormat, Status: record.Status, InstalledAt: record.InstalledAt, VerifiedAt: record.VerifiedAt}
		}
	}
	if data, err := os.ReadFile(u.Paths.Lock()); err == nil {
		status.Lock = data
	}
	status.LastTransaction = u.lastTransaction()
	status.Rollout = u.latestRollout()
	if status.Rollout != nil && (status.Rollout.Status == "running" || status.Rollout.Status == "degraded") {
		status.Desired = status.Rollout.Plan.Version
	} else {
		status.Desired = status.Current
	}
	if verify {
		if err := u.Verify(status.Current); err != nil {
			status.Integrity = "failed: " + err.Error()
		} else {
			status.Integrity = "verified"
		}
	}
	return status, nil
}

func (u *Updater) Prune(keep int) ([]string, error) {
	if keep < 2 {
		return nil, fmt.Errorf("prune must keep at least two verified releases")
	}
	lock, err := AcquireLock(u.Paths.Lock(), "prune", "")
	if err != nil {
		return nil, err
	}
	defer lock.Release()
	current, _ := u.CurrentVersion()
	type item struct {
		name     string
		verified time.Time
	}
	var values []item
	entries, _ := os.ReadDir(u.Paths.Releases())
	for _, entry := range entries {
		if !entry.IsDir() || !exactVersion.MatchString(entry.Name()) {
			continue
		}
		record, err := u.loadVerifiedRecord(entry.Name())
		if err != nil || record.VerifiedAt == nil {
			continue
		}
		values = append(values, item{entry.Name(), *record.VerifiedAt})
	}
	sort.Slice(values, func(i, j int) bool { return values[i].verified.After(values[j].verified) })
	protected := map[string]bool{current: true}
	for index, value := range values {
		if index < keep {
			protected[value.name] = true
		}
	}
	var removed []string
	for _, value := range values {
		if protected[value.name] {
			continue
		}
		releaseRoot := filepath.Join(u.Paths.Releases(), value.name)
		if releaseInUse(releaseRoot) {
			continue
		}
		if err := os.RemoveAll(releaseRoot); err != nil {
			return removed, err
		}
		_ = os.Remove(filepath.Join(u.Paths.Verified(), value.name+".json"))
		removed = append(removed, value.name)
	}
	artifacts, err := u.pruneArtifactCache(time.Now().UTC())
	if err != nil {
		return removed, err
	}
	for _, artifact := range artifacts {
		removed = append(removed, "artifact:"+artifact)
	}
	return removed, nil
}

func (u *Updater) pruneArtifactCache(now time.Time) ([]string, error) {
	entries, err := os.ReadDir(u.Paths.CacheRoot)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	cutoff := now.Add(-artifactCacheRetention)
	var removed []string
	for _, entry := range entries {
		if !entry.Type().IsRegular() {
			continue
		}
		info, infoErr := entry.Info()
		if infoErr != nil {
			return removed, infoErr
		}
		if info.ModTime().After(cutoff) {
			continue
		}
		if err = os.Remove(filepath.Join(u.Paths.CacheRoot, entry.Name())); err != nil {
			return removed, err
		}
		removed = append(removed, entry.Name())
	}
	sort.Strings(removed)
	return removed, nil
}

func (u *Updater) RecoverStale() ([]string, error) {
	if err := UnlockStale(u.Paths.Lock()); err != nil {
		return nil, err
	}
	entries, err := os.ReadDir(u.Paths.Releases())
	if err != nil && !os.IsNotExist(err) {
		return nil, err
	}
	var removed []string
	for _, entry := range entries {
		if entry.IsDir() && strings.HasPrefix(entry.Name(), ".staging-v") {
			path := filepath.Join(u.Paths.Releases(), entry.Name())
			if err = os.RemoveAll(path); err != nil {
				return removed, err
			}
			removed = append(removed, entry.Name())
		}
	}
	if tx := u.lastTransaction(); tx != nil && tx.Status == "running" {
		tx.Status = "interrupted"
		tx.Error = "recovered after stale transaction lock"
		tx.FinishedAt = time.Now().UTC()
		_ = u.writeTransaction(tx)
	}
	return removed, nil
}

func (u *Updater) prepare() error {
	for _, item := range []struct {
		path string
		mode os.FileMode
	}{{u.Paths.Releases(), 0o755}, {u.Paths.StateRoot, 0o700}, {u.Paths.CacheRoot, 0o700}, {u.Paths.Shared(), 0o700}, {u.Paths.ConfigRoot, 0o700}, {u.Paths.Transactions(), 0o700}, {u.Paths.Verified(), 0o700}} {
		if err := os.MkdirAll(item.path, item.mode); err != nil {
			return err
		}
	}
	return nil
}

func (u *Updater) downloadArtifact(ctx context.Context, artifact Artifact) (string, error) {
	if err := os.MkdirAll(u.Paths.CacheRoot, 0o700); err != nil {
		return "", err
	}
	path := filepath.Join(u.Paths.CacheRoot, artifact.SHA256+"."+strings.ReplaceAll(artifact.Format, ".", "-"))
	if digest, err := digestFile(path); err == nil && digest == artifact.SHA256 {
		return path, nil
	}
	temporary := path + fmt.Sprintf(".tmp-%d", os.Getpid())
	_ = os.Remove(temporary)
	if err := u.download(ctx, artifact.URL, temporary); err != nil {
		return "", err
	}
	digest, err := digestFile(temporary)
	if err != nil {
		_ = os.Remove(temporary)
		return "", err
	}
	if digest != artifact.SHA256 {
		_ = os.Remove(temporary)
		return "", fmt.Errorf("artifact SHA-256 mismatch: got %s", digest)
	}
	if err = os.Rename(temporary, path); err != nil {
		_ = os.Remove(temporary)
		return "", err
	}
	return path, nil
}

func (u *Updater) fetchBytes(ctx context.Context, location string) ([]byte, error) {
	temporary, err := os.CreateTemp("", "workbench-manifest-*")
	if err != nil {
		return nil, err
	}
	path := temporary.Name()
	temporary.Close()
	if err = os.Remove(path); err != nil {
		return nil, err
	}
	defer os.Remove(path)
	if err = u.download(ctx, location, path); err != nil {
		return nil, err
	}
	return os.ReadFile(path)
}

func (u *Updater) download(ctx context.Context, location, destination string) error {
	parsed, err := url.Parse(location)
	if err != nil {
		return err
	}
	if source, local := localSourcePath(location, parsed); local {
		input, err := os.Open(source)
		if err != nil {
			return err
		}
		defer input.Close()
		output, err := os.OpenFile(destination, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
		if err != nil {
			return err
		}
		_, copyErr := io.Copy(output, input)
		closeErr := output.Close()
		if copyErr != nil {
			return copyErr
		}
		return closeErr
	}
	if parsed.Scheme != "https" && parsed.Scheme != "http" {
		return fmt.Errorf("unsupported URL scheme %s", parsed.Scheme)
	}
	headers := make(http.Header)
	if u.Policy.Headers != nil {
		headers = u.Policy.Headers(parsed)
	}
	redactedLocation := parsed.Scheme + "://" + parsed.Host + parsed.Path
	var lastErr error
	for attempt := 1; attempt <= downloadMaxAttempts; attempt++ {
		retry, err := u.downloadRemoteOnce(ctx, location, redactedLocation, destination, headers)
		if err == nil {
			return nil
		}
		lastErr = err
		if !retry || attempt == downloadMaxAttempts {
			return err
		}
		delay := u.downloadRetryDelay * time.Duration(1<<(attempt-1))
		if delay > downloadMaxRetryDelay {
			delay = downloadMaxRetryDelay
		}
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
	return lastErr
}

func (u *Updater) downloadRemoteOnce(ctx context.Context, location, redactedLocation, destination string, headers http.Header) (bool, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, location, nil)
	if err != nil {
		return false, err
	}
	request.Header = headers.Clone()
	response, err := u.HTTPClient.Do(request)
	if err != nil {
		return ctx.Err() == nil, fmt.Errorf("download %s: %w", redactedLocation, err)
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		retry := response.StatusCode == http.StatusTooManyRequests || response.StatusCode == http.StatusRequestTimeout || response.StatusCode >= 500
		return retry, fmt.Errorf("download %s returned %s", redactedLocation, response.Status)
	}
	output, err := os.OpenFile(destination, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return false, err
	}
	_, copyErr := io.Copy(output, response.Body)
	closeErr := output.Close()
	if copyErr != nil || closeErr != nil {
		_ = os.Remove(destination)
		if copyErr != nil {
			return true, fmt.Errorf("download %s body: %w", redactedLocation, copyErr)
		}
		return true, closeErr
	}
	return false, nil
}

func localSourcePath(location string, parsed *url.URL) (string, bool) {
	if parsed.Scheme == "" {
		return location, true
	}
	// net/url interprets a Windows drive letter as a URL scheme. Keep local
	// bootstrap indexes local on every platform instead of rejecting C:\\...
	// as an unsupported "c" scheme.
	if len(location) >= 3 && location[1] == ':' && (location[2] == '\\' || location[2] == '/') {
		return location, true
	}
	if parsed.Scheme != "file" {
		return "", false
	}
	source := parsed.Path
	if runtime.GOOS == "windows" && len(source) >= 3 && source[0] == '/' && source[2] == ':' {
		source = source[1:]
	}
	return filepath.FromSlash(source), true
}

func resolveManifestReferences(manifestLocation string, manifest *Manifest) error {
	for platform, artifact := range manifest.Artifacts {
		resolved, err := resolveReference(manifestLocation, artifact.URL)
		if err != nil {
			return fmt.Errorf("resolve artifact URL for %s: %w", platform, err)
		}
		artifact.URL = resolved
		manifest.Artifacts[platform] = artifact
	}
	for dependencyIndex := range manifest.Dependencies {
		for platform, artifact := range manifest.Dependencies[dependencyIndex].Artifacts {
			resolved, err := resolveReference(manifestLocation, artifact.URL)
			if err != nil {
				return fmt.Errorf("resolve dependency %s URL for %s: %w", manifest.Dependencies[dependencyIndex].Name, platform, err)
			}
			artifact.URL = resolved
			manifest.Dependencies[dependencyIndex].Artifacts[platform] = artifact
		}
	}
	return nil
}

func resolveReference(baseLocation, reference string) (string, error) {
	if filepath.IsAbs(reference) {
		return reference, nil
	}
	parsedReference, err := url.Parse(reference)
	if err != nil {
		return "", err
	}
	if parsedReference.IsAbs() {
		return reference, nil
	}
	if filepath.IsAbs(baseLocation) {
		return filepath.Clean(filepath.Join(filepath.Dir(baseLocation), filepath.FromSlash(reference))), nil
	}
	parsedBase, err := url.Parse(baseLocation)
	if err != nil {
		return "", err
	}
	if parsedBase.Scheme == "http" || parsedBase.Scheme == "https" {
		return parsedBase.ResolveReference(parsedReference).String(), nil
	}
	basePath := baseLocation
	if parsedBase.Scheme == "file" {
		basePath = parsedBase.Path
	} else if parsedBase.Scheme != "" {
		return "", fmt.Errorf("unsupported base URL scheme %s", parsedBase.Scheme)
	}
	return filepath.Clean(filepath.Join(filepath.Dir(basePath), filepath.FromSlash(reference))), nil
}

func extractArchive(path, format string, stripComponents int, destination string) error {
	switch format {
	case "tar.gz":
		return extractTarGz(path, stripComponents, destination)
	case "zip":
		return extractZip(path, stripComponents, destination)
	default:
		return fmt.Errorf("unsupported archive format %s", format)
	}
}

func stripArchivePath(name string, components int) (string, bool) {
	parts := strings.Split(strings.TrimSuffix(name, "/"), "/")
	if len(parts) <= components {
		return "", false
	}
	return strings.Join(parts[components:], "/"), true
}

func cleanArchivePath(destination, name string) (string, error) {
	if name == "" || filepath.IsAbs(name) || strings.Contains(name, "\\") {
		return "", fmt.Errorf("unsafe archive path %q", name)
	}
	clean := filepath.Clean(filepath.FromSlash(name))
	if clean == "." || clean == ".." || strings.HasPrefix(clean, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("unsafe archive path %q", name)
	}
	path := filepath.Join(destination, clean)
	relative, err := filepath.Rel(destination, path)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("archive path escapes destination")
	}
	return path, nil
}

func extractTarGz(path string, stripComponents int, destination string) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	gz, err := gzip.NewReader(file)
	if err != nil {
		return err
	}
	defer gz.Close()
	reader := tar.NewReader(gz)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		name, keep := stripArchivePath(header.Name, stripComponents)
		if !keep {
			continue
		}
		target, err := cleanArchivePath(destination, name)
		if err != nil {
			return err
		}
		switch header.Typeflag {
		case tar.TypeDir:
			if err = os.MkdirAll(target, 0o755); err != nil {
				return err
			}
		case tar.TypeReg:
			if err = writeExtracted(target, reader, os.FileMode(header.Mode)); err != nil {
				return err
			}
		default:
			return fmt.Errorf("archive contains forbidden link or special file %q", header.Name)
		}
	}
	return nil
}

func extractZip(path string, stripComponents int, destination string) error {
	reader, err := zip.OpenReader(path)
	if err != nil {
		return err
	}
	defer reader.Close()
	for _, file := range reader.File {
		name, keep := stripArchivePath(file.Name, stripComponents)
		if !keep {
			continue
		}
		target, err := cleanArchivePath(destination, name)
		if err != nil {
			return err
		}
		if file.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("archive contains forbidden symlink %q", file.Name)
		}
		if file.FileInfo().IsDir() {
			if err = os.MkdirAll(target, 0o755); err != nil {
				return err
			}
			continue
		}
		input, err := file.Open()
		if err != nil {
			return err
		}
		err = writeExtracted(target, input, file.Mode())
		input.Close()
		if err != nil {
			return err
		}
	}
	return nil
}

func writeExtracted(target string, reader io.Reader, mode os.FileMode) error {
	if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return err
	}
	mode &= 0o755
	if mode == 0 {
		mode = 0o644
	}
	file, err := os.OpenFile(target, os.O_CREATE|os.O_EXCL|os.O_WRONLY, mode)
	if err != nil {
		return err
	}
	_, copyErr := io.Copy(file, reader)
	closeErr := file.Close()
	if copyErr != nil {
		return copyErr
	}
	return closeErr
}

func verifyFiles(root string, files map[string]string) error {
	for relative, expected := range files {
		path, err := cleanArchivePath(root, relative)
		if err != nil {
			return err
		}
		digest, err := digestFile(path)
		if err != nil {
			return fmt.Errorf("verify %s: %w", relative, err)
		}
		if digest != expected {
			return fmt.Errorf("file %s SHA-256 mismatch", relative)
		}
	}
	return nil
}

func digestFile(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	hash := sha256.New()
	if _, err = io.Copy(hash, file); err != nil {
		return "", err
	}
	return hex.EncodeToString(hash.Sum(nil)), nil
}
func digestBytes(data []byte) string { sum := sha256.Sum256(data); return hex.EncodeToString(sum[:]) }

func compareVersion(left, right string) int {
	var leftMajor, leftMinor, leftPatch int
	var rightMajor, rightMinor, rightPatch int
	left = strings.TrimPrefix(strings.SplitN(left, "-", 2)[0], "v")
	right = strings.TrimPrefix(strings.SplitN(right, "-", 2)[0], "v")
	if _, err := fmt.Sscanf(left, "%d.%d.%d", &leftMajor, &leftMinor, &leftPatch); err != nil {
		return -1
	}
	if _, err := fmt.Sscanf(right, "%d.%d.%d", &rightMajor, &rightMinor, &rightPatch); err != nil {
		return 1
	}
	leftParts := []int{leftMajor, leftMinor, leftPatch}
	rightParts := []int{rightMajor, rightMinor, rightPatch}
	for index := range leftParts {
		if leftParts[index] < rightParts[index] {
			return -1
		}
		if leftParts[index] > rightParts[index] {
			return 1
		}
	}
	return 0
}

func (u *Updater) healthCheck(parent context.Context, release string, check *HealthCheck) error {
	command, err := cleanArchivePath(release, check.Command)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(parent, time.Duration(check.TimeoutSeconds)*time.Second)
	defer cancel()
	var process *exec.Cmd
	if runtime.GOOS == "windows" && strings.EqualFold(filepath.Ext(command), ".cmd") {
		process = exec.CommandContext(ctx, "cmd.exe", append([]string{"/d", "/s", "/c", command}, check.Args...)...)
	} else {
		process = exec.CommandContext(ctx, command, check.Args...)
	}
	process.Dir = release
	process.Stdout = u.Stdout
	process.Stderr = u.Stderr
	if err = process.Run(); ctx.Err() == context.DeadlineExceeded {
		return fmt.Errorf("timed out after %ds", check.TimeoutSeconds)
	}
	return err
}

func (u *Updater) installLaunchers(commands map[string]string) error {
	if err := os.MkdirAll(u.Paths.BinRoot, 0o755); err != nil {
		return err
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	self, err = filepath.EvalSymlinks(self)
	if err != nil {
		return err
	}
	for name := range commands {
		if strings.ContainsAny(name, "/\\:") || name == "" {
			return fmt.Errorf("invalid command name %q", name)
		}
		path := filepath.Join(u.Paths.BinRoot, name)
		temporary := path + fmt.Sprintf(".tmp-%d", os.Getpid())
		_ = os.Remove(temporary)
		if runtime.GOOS == "windows" {
			path += ".cmd"
			temporary += ".cmd"
			body := fmt.Sprintf("@echo off\r\n\"%s\" exec %s -- %%*\r\n", self, name)
			if err = os.WriteFile(temporary, []byte(body), 0o644); err != nil {
				return err
			}
		} else {
			body := fmt.Sprintf("#!/bin/sh\nexec %s exec %s -- \"$@\"\n", shellQuote(self), shellQuote(name))
			if err = os.WriteFile(temporary, []byte(body), 0o755); err != nil {
				return err
			}
		}
		if err = replacePath(temporary, path); err != nil {
			_ = os.Remove(temporary)
			return err
		}
	}
	return nil
}

func shellQuote(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'" }

func (u *Updater) writeTransaction(tx *transaction) error {
	if err := os.MkdirAll(u.Paths.Transactions(), 0o700); err != nil {
		return err
	}
	data, _ := json.MarshalIndent(tx, "", "  ")
	path := filepath.Join(u.Paths.Transactions(), tx.ID+".json")
	return os.WriteFile(path, append(data, '\n'), 0o600)
}

func (u *Updater) fail(tx *transaction, err error) error {
	tx.Status = "failed"
	tx.Error = err.Error()
	tx.FinishedAt = time.Now().UTC()
	_ = u.writeTransaction(tx)
	return err
}

func (u *Updater) lastTransaction() *transaction {
	entries, err := os.ReadDir(u.Paths.Transactions())
	if err != nil {
		return nil
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() > entries[j].Name() })
	for _, entry := range entries {
		data, err := os.ReadFile(filepath.Join(u.Paths.Transactions(), entry.Name()))
		if err != nil {
			continue
		}
		var tx transaction
		if json.Unmarshal(data, &tx) == nil {
			return &tx
		}
	}
	return nil
}

func (u *Updater) writeVerifiedRecord(record releaseRecord) error {
	if err := os.MkdirAll(u.Paths.Verified(), 0o700); err != nil {
		return err
	}
	data, _ := json.MarshalIndent(record, "", "  ")
	return os.WriteFile(filepath.Join(u.Paths.Verified(), record.Version+".json"), append(data, '\n'), 0o600)
}

func (u *Updater) loadVerifiedRecord(version string) (*releaseRecord, error) {
	data, err := os.ReadFile(filepath.Join(u.Paths.Verified(), version+".json"))
	if err != nil {
		return nil, err
	}
	var record releaseRecord
	if err = json.Unmarshal(data, &record); err != nil {
		return nil, err
	}
	if record.PackageID != u.Policy.PackageID && !(u.Policy.AllowLegacyIdentity && record.PackageID == "") {
		return nil, fmt.Errorf("verified package identity mismatch")
	}
	if record.Version != version {
		return nil, fmt.Errorf("verified release record version mismatch")
	}
	return &record, nil
}

func (u *Updater) parseManifest(data []byte) (*Manifest, error) {
	manifest, err := ParseManifestForSchema(data, u.Policy.ManifestSchema)
	if err != nil {
		return nil, err
	}
	if manifest.PackageID != u.Policy.PackageID && !(u.Policy.AllowLegacyIdentity && manifest.PackageID == "") {
		return nil, fmt.Errorf("package identity mismatch: expected %s, got %s", u.Policy.PackageID, manifest.PackageID)
	}
	for _, platform := range u.Policy.RequiredPlatforms {
		if _, ok := manifest.Artifacts[platform]; !ok {
			return nil, fmt.Errorf("manifest is missing artifact for %s", platform)
		}
	}
	return manifest, nil
}
