package updater

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func testPaths(root string) Paths {
	return Paths{DataRoot: filepath.Join(root, "data"), StateRoot: filepath.Join(root, "state"), CacheRoot: filepath.Join(root, "cache"), ConfigRoot: filepath.Join(root, "config"), BinRoot: filepath.Join(root, "bin")}
}

func TestDefaultReleaseDownloadTimeoutSupportsCrossRegionArtifacts(t *testing.T) {
	client := New(testPaths(t.TempDir()))
	if client.HTTPClient.Timeout != 30*time.Minute {
		t.Fatalf("download timeout=%s, want %s", client.HTTPClient.Timeout, 30*time.Minute)
	}
}

func TestPruneArtifactCacheRetainsOnlyLastTwentyFourHours(t *testing.T) {
	paths := testPaths(t.TempDir())
	if err := os.MkdirAll(paths.CacheRoot, 0o700); err != nil {
		t.Fatal(err)
	}
	now := time.Date(2026, 9, 2, 12, 0, 0, 0, time.UTC)
	oldPath := filepath.Join(paths.CacheRoot, "old.tar-gz")
	boundaryPath := filepath.Join(paths.CacheRoot, "boundary.tar-gz")
	recentPath := filepath.Join(paths.CacheRoot, "recent.tar-gz")
	for _, path := range []string{oldPath, boundaryPath, recentPath} {
		if err := os.WriteFile(path, []byte("artifact"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Chtimes(oldPath, now.Add(-25*time.Hour), now.Add(-25*time.Hour)); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(boundaryPath, now.Add(-24*time.Hour), now.Add(-24*time.Hour)); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(recentPath, now.Add(-23*time.Hour), now.Add(-23*time.Hour)); err != nil {
		t.Fatal(err)
	}
	removed, err := New(paths).pruneArtifactCache(now)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(removed, ",") != "boundary.tar-gz,old.tar-gz" {
		t.Fatalf("removed=%v, want boundary and old artifacts", removed)
	}
	if _, err := os.Stat(recentPath); err != nil {
		t.Fatalf("recent artifact was removed: %v", err)
	}
}

func TestDownloadRetriesTransientHTTPFailures(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		attempt := requests.Add(1)
		if attempt == 1 {
			http.Error(writer, "busy", http.StatusTooManyRequests)
			return
		}
		if attempt == 2 {
			http.Error(writer, "temporary", http.StatusServiceUnavailable)
			return
		}
		_, _ = writer.Write([]byte("release"))
	}))
	defer server.Close()

	client := New(testPaths(t.TempDir()))
	client.downloadRetryDelay = time.Millisecond
	destination := filepath.Join(t.TempDir(), "release.bin")
	if err := client.download(context.Background(), server.URL+"/release.bin", destination); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 3 {
		t.Fatalf("requests=%d, want 3", requests.Load())
	}
	data, err := os.ReadFile(destination)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "release" {
		t.Fatalf("downloaded %q, want release", data)
	}
}

func TestDownloadDoesNotRetryPermanentHTTPFailure(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		requests.Add(1)
		http.NotFound(writer, request)
	}))
	defer server.Close()

	client := New(testPaths(t.TempDir()))
	client.downloadRetryDelay = time.Millisecond
	err := client.download(context.Background(), server.URL+"/missing", filepath.Join(t.TempDir(), "missing"))
	if err == nil || !strings.Contains(err.Error(), "404 Not Found") {
		t.Fatalf("error=%v, want 404", err)
	}
	if requests.Load() != 1 {
		t.Fatalf("requests=%d, want 1", requests.Load())
	}
}

func TestDownloadBoundsTransientRetries(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		requests.Add(1)
		http.Error(writer, "temporary", http.StatusServiceUnavailable)
	}))
	defer server.Close()

	client := New(testPaths(t.TempDir()))
	client.downloadRetryDelay = 0
	err := client.download(context.Background(), server.URL, filepath.Join(t.TempDir(), "release"))
	if err == nil || !strings.Contains(err.Error(), "503 Service Unavailable") {
		t.Fatalf("error=%v, want 503", err)
	}
	if requests.Load() != downloadMaxAttempts {
		t.Fatalf("requests=%d, want %d", requests.Load(), downloadMaxAttempts)
	}
}

func TestStatusIgnoresTerminalRolloutAsDesiredVersion(t *testing.T) {
	paths := testPaths(t.TempDir())
	client := New(paths)
	if err := os.MkdirAll(filepath.Join(paths.Releases(), "v1.0.2"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := client.setCurrent("v1.0.2"); err != nil {
		t.Fatal(err)
	}
	rollout := &RolloutState{ID: "terminal", Plan: RolloutPlan{Version: "v1.0.1"}, Status: "complete"}
	if err := client.writeRollout(rollout); err != nil {
		t.Fatal(err)
	}
	status, err := client.Status(false)
	if err != nil {
		t.Fatal(err)
	}
	if status.Desired != "v1.0.2" {
		t.Fatalf("desired=%q, want current terminal version v1.0.2", status.Desired)
	}
	rollout.Status = "degraded"
	if err := client.writeRollout(rollout); err != nil {
		t.Fatal(err)
	}
	status, err = client.Status(false)
	if err != nil {
		t.Fatal(err)
	}
	if status.Desired != "v1.0.1" {
		t.Fatalf("desired=%q, want active rollout version v1.0.1", status.Desired)
	}
}

func makeArchive(t *testing.T, root, name, script string) (string, string) {
	t.Helper()
	path := filepath.Join(root, name+".tar.gz")
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	gz := gzip.NewWriter(file)
	tarWriter := tar.NewWriter(gz)
	header := &tar.Header{Name: "bin/health", Mode: 0o755, Size: int64(len(script)), ModTime: time.Unix(0, 0)}
	if err = tarWriter.WriteHeader(header); err != nil {
		t.Fatal(err)
	}
	if _, err = tarWriter.Write([]byte(script)); err != nil {
		t.Fatal(err)
	}
	if err = tarWriter.Close(); err != nil {
		t.Fatal(err)
	}
	if err = gz.Close(); err != nil {
		t.Fatal(err)
	}
	if err = file.Close(); err != nil {
		t.Fatal(err)
	}
	digest, err := digestFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return path, digest
}

func makeManifest(t *testing.T, root, version, archive, archiveDigest, scriptDigest string) string {
	t.Helper()
	artifact := Artifact{URL: archive, SHA256: archiveDigest, Format: "tar.gz"}
	manifest := Manifest{PackageID: "distributed-workbench", SchemaVersion: ManifestSchema, Version: version, StateFormat: 1, Status: "supported", Compatibility: Compatibility{Controller: ">=v1.0.0 <v2.0.0", Executor: ">=v1.0.0 <v2.0.0"}, Artifacts: map[string]Artifact{"darwin-arm64": artifact, "linux-amd64": artifact, "windows-amd64": artifact}, Commands: map[string]string{"workbench-health": "bin/health"}, HealthCheck: &HealthCheck{Command: "bin/health", TimeoutSeconds: 2}, Files: map[string]string{"bin/health": scriptDigest}}
	data, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(root, version+".json")
	if err = os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestInstallRollbackVerifyAndPrune(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	paths := testPaths(root)
	client := New(paths)
	versions := []string{"v1.0.0", "v1.0.1", "v1.0.2"}
	for _, version := range versions {
		script := []byte("#!/bin/sh\nexit 0\n")
		sum := sha256.Sum256(script)
		scriptDigest := hex.EncodeToString(sum[:])
		archive, archiveDigest := makeArchive(t, root, version, string(script))
		manifest := makeManifest(t, root, version, archive, archiveDigest, scriptDigest)
		if err := client.Install(context.Background(), version, manifest, "", false, false, ""); err != nil {
			t.Fatalf("install %s: %v", version, err)
		}
	}
	current, err := client.CurrentVersion()
	if err != nil || current != "v1.0.2" {
		t.Fatalf("current=%q err=%v", current, err)
	}
	if err = client.Rollback(context.Background(), "v1.0.1", false, ""); err != nil {
		t.Fatal(err)
	}
	if err = client.Verify(""); err != nil {
		t.Fatal(err)
	}
	removed, err := client.Prune(2)
	if err != nil {
		t.Fatal(err)
	}
	if len(removed) != 1 || removed[0] != "v1.0.0" {
		t.Fatalf("removed %#v", removed)
	}
}

func TestInstallReactivatesMatchingVerifiedImmutableRelease(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	manifests := map[string]string{}
	for _, version := range []string{"v1.0.0", "v1.0.1"} {
		script := []byte("#!/bin/sh\nexit 0\n")
		sum := sha256.Sum256(script)
		archive, archiveDigest := makeArchive(t, root, version, string(script))
		manifest := makeManifest(t, root, version, archive, archiveDigest, hex.EncodeToString(sum[:]))
		manifests[version] = manifest
		if err := client.Install(context.Background(), version, manifest, "", false, false, ""); err != nil {
			t.Fatal(err)
		}
	}
	if err := client.Rollback(context.Background(), "v1.0.0", false, ""); err != nil {
		t.Fatal(err)
	}
	if err := client.Install(context.Background(), "v1.0.1", manifests["v1.0.1"], "", false, false, ""); err != nil {
		t.Fatalf("reactivate verified immutable release: %v", err)
	}
	current, err := client.CurrentVersion()
	if err != nil || current != "v1.0.1" {
		t.Fatalf("current=%q err=%v", current, err)
	}
}

func TestBootstrapRepairsMissingVerifiedRecordForCurrentRelease(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	script := []byte("#!/bin/sh\nexit 0\n")
	sum := sha256.Sum256(script)
	archive, archiveDigest := makeArchive(t, root, "v1.0.0", string(script))
	manifestPath := makeManifest(t, root, "v1.0.0", archive, archiveDigest, hex.EncodeToString(sum[:]))
	manifestData, err := os.ReadFile(manifestPath)
	if err != nil {
		t.Fatal(err)
	}
	manifestDigest := digestBytes(manifestData)
	if err := client.Install(context.Background(), "v1.0.0", manifestPath, manifestDigest, false, false, ""); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(client.Paths.Verified(), "v1.0.0.json")); err != nil {
		t.Fatal(err)
	}
	index := []byte(`{"schemaVersion":"distributed-workbench.recommended/v1","updaterVersion":"dev","workbenchVersion":"v1.0.0","manifestUrl":"` + filepath.ToSlash(manifestPath) + `","manifestSha256":"` + manifestDigest + `"}`)
	indexPath := filepath.Join(root, "recommended.json")
	if err := os.WriteFile(indexPath, index, 0o600); err != nil {
		t.Fatal(err)
	}
	indexSum := sha256.Sum256(index)
	if err := client.Bootstrap(context.Background(), indexPath, hex.EncodeToString(indexSum[:])); err != nil {
		t.Fatalf("bootstrap did not repair current verified record: %v", err)
	}
	record, err := client.loadVerifiedRecord("v1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	if record.ManifestDigest != manifestDigest || record.VerifiedAt == nil || record.Status != "verified" {
		t.Fatalf("unexpected repaired record: %#v", record)
	}
}

func TestInstallWritesVerifiedDependencyMetadata(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	script := []byte("#!/bin/sh\nexit 0\n")
	sum := sha256.Sum256(script)
	archive, archiveDigest := makeArchive(t, root, "main", string(script))
	dependencyArchive, dependencyDigest := makeArchive(t, root, "dependency", string(script))
	manifestPath := makeManifest(t, root, "v1.0.0", archive, archiveDigest, hex.EncodeToString(sum[:]))
	manifestData, err := os.ReadFile(manifestPath)
	if err != nil {
		t.Fatal(err)
	}
	var manifest Manifest
	if err = json.Unmarshal(manifestData, &manifest); err != nil {
		t.Fatal(err)
	}
	dependencyArtifact := Artifact{URL: dependencyArchive, SHA256: dependencyDigest, Format: "tar.gz"}
	manifest.Dependencies = []Dependency{{
		Name: "distributed-workbench", Version: "v0.3.28",
		Repository: "https://github.com/lukewang1024/distributed-workbench.git",
		Revision:   "952892666e1dea64740645c0069f9dfcc5731314",
		Artifacts:  map[string]Artifact{"darwin-arm64": dependencyArtifact, "linux-amd64": dependencyArtifact, "windows-amd64": dependencyArtifact},
	}}
	manifestData, _ = json.MarshalIndent(manifest, "", "  ")
	if err = os.WriteFile(manifestPath, manifestData, 0o600); err != nil {
		t.Fatal(err)
	}
	if err = client.Install(context.Background(), "v1.0.0", manifestPath, "", false, false, ""); err != nil {
		t.Fatal(err)
	}
	metadataPath := filepath.Join(client.Paths.Releases(), "v1.0.0", "dependencies", "distributed-workbench", ".workbench-artifact.json")
	metadata, err := os.ReadFile(metadataPath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(metadata), manifest.Dependencies[0].Revision) {
		t.Fatalf("unexpected metadata: %s", metadata)
	}
	if err = client.Verify(""); err != nil {
		t.Fatal(err)
	}
}

func TestFailedHealthCheckRestoresCurrent(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	install := func(version, exit string) error {
		script := []byte("#!/bin/sh\nexit " + exit + "\n")
		sum := sha256.Sum256(script)
		archive, digest := makeArchive(t, root, version, string(script))
		return client.Install(context.Background(), version, makeManifest(t, root, version, archive, digest, hex.EncodeToString(sum[:])), "", false, false, "")
	}
	if err := install("v1.0.0", "0"); err != nil {
		t.Fatal(err)
	}
	if err := install("v1.0.1", "7"); err == nil {
		t.Fatal("expected failed health check")
	}
	if _, err := os.Stat(filepath.Join(client.Paths.Releases(), "v1.0.1")); !os.IsNotExist(err) {
		t.Fatalf("failed release was retained: %v", err)
	}
	current, err := client.CurrentVersion()
	if err != nil || current != "v1.0.0" {
		t.Fatalf("current=%q err=%v", current, err)
	}
}

func TestRolloutFailureStopsAndVerifiesAutomaticRollback(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	build := func(version, exit string) (string, string) {
		script := []byte("#!/bin/sh\nexit " + exit + "\n")
		sum := sha256.Sum256(script)
		archive, archiveDigest := makeArchive(t, root, version, string(script))
		manifest := makeManifest(t, root, version, archive, archiveDigest, hex.EncodeToString(sum[:]))
		digest, err := digestFile(manifest)
		if err != nil {
			t.Fatal(err)
		}
		return manifest, digest
	}
	oldManifest, _ := build("v1.0.0", "0")
	if err := client.Install(context.Background(), "v1.0.0", oldManifest, "", false, false, ""); err != nil {
		t.Fatal(err)
	}
	badManifest, badDigest := build("v1.0.1", "7")
	plan := RolloutPlan{
		SchemaVersion: "distributed-workbench.rollout/v1", Version: "v1.0.1",
		RollbackVersion: "v1.0.0", ManifestURL: badManifest, ManifestSHA256: badDigest,
		Nodes:       []RolloutNode{{ID: "local", Platform: "darwin-arm64", Role: "controller", Local: true}},
		FabricCheck: []string{"true"},
	}
	planData, _ := json.Marshal(plan)
	planPath := filepath.Join(root, "rollout.json")
	if err := os.WriteFile(planPath, planData, 0o600); err != nil {
		t.Fatal(err)
	}
	state, err := client.StartRollout(context.Background(), planPath)
	if err == nil {
		t.Fatal("expected rollout failure")
	}
	if state.Status != "rolled-back" {
		t.Fatalf("status=%s events=%#v", state.Status, state.Events)
	}
	current, currentErr := client.CurrentVersion()
	if currentErr != nil || current != "v1.0.0" {
		t.Fatalf("current=%q err=%v", current, currentErr)
	}
	if verifyErr := client.Verify("v1.0.0"); verifyErr != nil {
		t.Fatal(verifyErr)
	}
	found := false
	for _, event := range state.Events {
		if event.Type == "rollback-verified" && event.Node == "local" {
			found = true
		}
	}
	if !found {
		t.Fatalf("rollback verification event missing: %#v", state.Events)
	}
}

func TestRolloutPreflightRejectsRollbackVersionDriftBeforeInstall(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	script := []byte("#!/bin/sh\nexit 0\n")
	sum := sha256.Sum256(script)
	archive, archiveDigest := makeArchive(t, root, "v1.0.0", string(script))
	manifest := makeManifest(t, root, "v1.0.0", archive, archiveDigest, hex.EncodeToString(sum[:]))
	if err := client.Install(context.Background(), "v1.0.0", manifest, "", false, false, ""); err != nil {
		t.Fatal(err)
	}
	plan := RolloutPlan{
		SchemaVersion: "distributed-workbench.rollout/v1", Version: "v1.0.2",
		RollbackVersion: "v1.0.1", ManifestURL: manifest, ManifestSHA256: strings.Repeat("a", 64),
		Nodes: []RolloutNode{{ID: "local", Platform: "darwin-arm64", Role: "controller", Local: true}}, FabricCheck: []string{"true"},
	}
	data, _ := json.Marshal(plan)
	path := filepath.Join(root, "rollout-preflight.json")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	state, err := client.StartRollout(context.Background(), path)
	if err == nil || state != nil || !strings.Contains(err.Error(), "before any install") && !strings.Contains(err.Error(), "expected rollbackVersion") {
		t.Fatalf("state=%#v err=%v", state, err)
	}
	if current, currentErr := client.CurrentVersion(); currentErr != nil || current != "v1.0.0" {
		t.Fatalf("current=%q err=%v", current, currentErr)
	}
}

func TestFabricReadinessWaitsForReconnect(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell readiness fixture is POSIX")
	}
	root := t.TempDir()
	counter := filepath.Join(root, "attempts")
	script := filepath.Join(root, "fabric-check")
	contents := "#!/bin/sh\nn=0\n[ ! -f \"$1\" ] || n=$(cat \"$1\")\nn=$((n + 1))\nprintf '%s\\n' \"$n\" > \"$1\"\n[ \"$n\" -ge 3 ]\n"
	if err := os.WriteFile(script, []byte(contents), 0o755); err != nil {
		t.Fatal(err)
	}
	client := New(testPaths(root))
	client.FabricCheckTimeout = time.Second
	client.FabricCheckInterval = 10 * time.Millisecond
	if output, err := client.waitForFabric(context.Background(), []string{script, counter}); err != nil {
		t.Fatalf("readiness did not converge: %v (%s)", err, output)
	}
	data, err := os.ReadFile(counter)
	if err != nil || strings.TrimSpace(string(data)) != "3" {
		t.Fatalf("attempts=%q err=%v", data, err)
	}
}

func TestFabricReadinessReceivesRolloutNodeScope(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell readiness fixture is POSIX")
	}
	root := t.TempDir()
	script := filepath.Join(root, "fabric-check")
	contents := "#!/bin/sh\ntest \"$WORKBENCH_ROLLOUT_NODE_IDS\" = \"windows,mac\"\n"
	if err := os.WriteFile(script, []byte(contents), 0o755); err != nil {
		t.Fatal(err)
	}
	client := New(testPaths(root))
	if output, err := client.waitForFabric(
		context.Background(),
		[]string{script},
		RolloutNode{ID: "windows"},
		RolloutNode{ID: "mac"},
	); err != nil {
		t.Fatalf("rollout scope was not propagated: %v (%s)", err, output)
	}
}

func TestExtractRejectsTraversalAndSymlink(t *testing.T) {
	root := t.TempDir()
	archive := filepath.Join(root, "bad.tar.gz")
	file, _ := os.Create(archive)
	gz := gzip.NewWriter(file)
	writer := tar.NewWriter(gz)
	_ = writer.WriteHeader(&tar.Header{Name: "../escape", Mode: 0o644, Size: 1})
	_, _ = writer.Write([]byte("x"))
	_ = writer.Close()
	_ = gz.Close()
	_ = file.Close()
	if err := extractTarGz(archive, 0, filepath.Join(root, "out")); err == nil {
		t.Fatal("expected traversal rejection")
	}
}

func TestExtractStripsValidatedTopLevelDirectory(t *testing.T) {
	root := t.TempDir()
	archive := filepath.Join(root, "dependency.tar.gz")
	file, _ := os.Create(archive)
	gz := gzip.NewWriter(file)
	writer := tar.NewWriter(gz)
	content := []byte("ok")
	_ = writer.WriteHeader(&tar.Header{Name: "dependency-v1/bin/tool", Mode: 0o755, Size: int64(len(content))})
	_, _ = writer.Write(content)
	_ = writer.Close()
	_ = gz.Close()
	_ = file.Close()
	destination := filepath.Join(root, "out")
	if err := extractTarGz(archive, 1, destination); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(filepath.Join(destination, "bin", "tool"))
	if err != nil || string(got) != "ok" {
		t.Fatalf("content=%q err=%v", got, err)
	}
}

func TestLockFailsFastAndRequiresStaleOwner(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lock")
	lock, err := AcquireLock(path, "install", "v1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	defer lock.Release()
	if _, err = AcquireLock(path, "rollback", "v1.0.0"); err == nil {
		t.Fatal("expected lock contention")
	}
	if err = UnlockStale(path); err == nil {
		t.Fatal("must not unlock a live owner")
	}
}

func TestRolloutOrdersExecutorsBeforeController(t *testing.T) {
	data := []byte(`{"schemaVersion":"distributed-workbench.rollout/v1","version":"v1.2.3","rollbackVersion":"v1.2.2","manifestUrl":"https://example.test/manifest.json","manifestSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nodes":[{"id":"controller","platform":"darwin-arm64","role":"controller","local":true},{"id":"windows","platform":"windows-amd64","role":"executor","address":"win"},{"id":"linux","platform":"linux-amd64","role":"executor","address":"linux"}],"fabricCheck":["true"]}`)
	plan, err := ParseRolloutPlan(data)
	if err != nil {
		t.Fatal(err)
	}
	ordered := orderRolloutNodes(plan.Nodes, plan.RolloutOrder)
	if ordered[0].ID != "windows" || ordered[1].ID != "linux" || ordered[2].ID != "controller" {
		t.Fatalf("unexpected order: %#v", ordered)
	}
}

func TestRolloutCanDeclareControllerFirstCompatibilityOrder(t *testing.T) {
	data := []byte(`{"schemaVersion":"distributed-workbench.rollout/v1","version":"v1.2.3","rollbackVersion":"v1.2.2","rolloutOrder":"controller-first","manifestUrl":"https://example.test/manifest.json","manifestSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nodes":[{"id":"controller","platform":"darwin-arm64","role":"controller","local":true},{"id":"linux","platform":"linux-amd64","role":"executor","address":"linux"}],"fabricCheck":["true"]}`)
	plan, err := ParseRolloutPlan(data)
	if err != nil {
		t.Fatal(err)
	}
	ordered := orderRolloutNodes(plan.Nodes, plan.RolloutOrder)
	if ordered[0].ID != "controller" || ordered[1].ID != "linux" {
		t.Fatalf("unexpected order: %#v", ordered)
	}
}

func TestRecoverStaleLockRemovesOnlyStagingDirectories(t *testing.T) {
	root := t.TempDir()
	client := New(testPaths(root))
	if err := os.MkdirAll(client.Paths.Releases(), 0o755); err != nil {
		t.Fatal(err)
	}
	staging := filepath.Join(client.Paths.Releases(), ".staging-v1.0.0-dead")
	release := filepath.Join(client.Paths.Releases(), "v1.0.0")
	_ = os.Mkdir(staging, 0o755)
	_ = os.Mkdir(release, 0o755)
	if err := os.MkdirAll(filepath.Dir(client.Paths.Lock()), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(client.Paths.Lock(), []byte(`{"pid":99999999,"operation":"install","version":"v1.0.0","transactionId":"dead","startedAt":"2026-01-01T00:00:00Z"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	removed, err := client.RecoverStale()
	if err != nil {
		t.Fatal(err)
	}
	if len(removed) != 1 {
		t.Fatalf("removed=%v", removed)
	}
	if _, err = os.Stat(release); err != nil {
		t.Fatalf("committed release removed: %v", err)
	}
}

func TestBootstrapRejectsUnexpectedUpdaterVersion(t *testing.T) {
	root := t.TempDir()
	index := []byte(`{"schemaVersion":"distributed-workbench.recommended/v1","updaterVersion":"v2.0.0","workbenchVersion":"v1.2.3","manifestUrl":"https://example.test/manifest.json","manifestSha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}`)
	path := filepath.Join(root, "recommended.json")
	if err := os.WriteFile(path, index, 0o600); err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(index)
	client := New(testPaths(root))
	client.Version = "v1.0.0"
	if err := client.Bootstrap(context.Background(), path, hex.EncodeToString(sum[:])); err == nil {
		t.Fatal("expected updater version mismatch")
	}
}

func TestReconcileVerifiesDesiredIndexSidecar(t *testing.T) {
	root := t.TempDir()
	index := []byte(`{"schemaVersion":"distributed-workbench.recommended/v1","updaterVersion":"v2.0.0","workbenchVersion":"v1.2.3","manifestUrl":"manifest.json","manifestSha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}`)
	indexPath := filepath.Join(root, "recommended.json")
	digestPath := indexPath + ".sha256"
	if err := os.WriteFile(indexPath, index, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(digestPath, []byte("not-a-digest\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	client := New(testPaths(root))
	client.Version = "v1.0.0"
	if err := client.Reconcile(context.Background(), indexPath, digestPath); err == nil || !strings.Contains(err.Error(), "digest sidecar") {
		t.Fatalf("expected sidecar rejection, got %v", err)
	}
	sum := sha256.Sum256(index)
	if err := os.WriteFile(digestPath, []byte(hex.EncodeToString(sum[:])+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := client.Reconcile(context.Background(), indexPath, digestPath); err == nil || !strings.Contains(err.Error(), "expects updater") {
		t.Fatalf("verified sidecar did not reach recommendation validation: %v", err)
	}
}

func TestInstallResolvesArtifactsRelativeToManifest(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell health fixture is POSIX")
	}
	root := t.TempDir()
	releaseDir := filepath.Join(root, "release")
	if err := os.Mkdir(releaseDir, 0o755); err != nil {
		t.Fatal(err)
	}
	script := []byte("#!/bin/sh\nexit 0\n")
	scriptSum := sha256.Sum256(script)
	archive, archiveDigest := makeArchive(t, releaseDir, "payload", string(script))
	artifact := Artifact{URL: filepath.Base(archive), SHA256: archiveDigest, Format: "tar.gz"}
	manifest := Manifest{PackageID: "distributed-workbench", SchemaVersion: ManifestSchema, Version: "v1.2.3", StateFormat: 1, Status: "supported", Compatibility: Compatibility{Controller: ">=v1.0.0 <v2.0.0", Executor: ">=v1.0.0 <v2.0.0"}, Artifacts: map[string]Artifact{"darwin-arm64": artifact, "linux-amd64": artifact, "windows-amd64": artifact}, Commands: map[string]string{"workbench-health": "bin/health"}, HealthCheck: &HealthCheck{Command: "bin/health", TimeoutSeconds: 2}, Files: map[string]string{"bin/health": hex.EncodeToString(scriptSum[:])}}
	data, err := json.Marshal(manifest)
	if err != nil {
		t.Fatal(err)
	}
	manifestPath := filepath.Join(releaseDir, "manifest.json")
	if err = os.WriteFile(manifestPath, data, 0o600); err != nil {
		t.Fatal(err)
	}
	client := New(testPaths(root))
	if err = client.Install(context.Background(), "v1.2.3", manifestPath, digestBytes(data), false, false, ""); err != nil {
		t.Fatal(err)
	}
}

func TestResolveReferenceForHTTPAndLocalBundles(t *testing.T) {
	httpValue, err := resolveReference("https://artifacts.example/releases/v1/manifest.json", "payload.tar.gz")
	if err != nil || httpValue != "https://artifacts.example/releases/v1/payload.tar.gz" {
		t.Fatalf("http reference=%q err=%v", httpValue, err)
	}
	localValue, err := resolveReference(filepath.Join("tmp", "release", "recommended.json"), "manifest.json")
	if err != nil || !strings.HasSuffix(filepath.ToSlash(localValue), "tmp/release/manifest.json") {
		t.Fatalf("local reference=%q err=%v", localValue, err)
	}
}

func TestLocalSourcePathRecognizesWindowsDrivePaths(t *testing.T) {
	location := `C:\Users\Administrator\AppData\Local\Temp\recommended.json`
	parsed, err := url.Parse(location)
	if err != nil {
		t.Fatal(err)
	}
	if source, local := localSourcePath(location, parsed); !local || source != location {
		t.Fatalf("source=%q local=%v", source, local)
	}
}

func TestComponentRolloutPreservesNodeOrder(t *testing.T) {
	plan := RolloutPlan{SchemaVersion: "distributed-workbench.rollout/v1", Version: "v1.2.3", RollbackVersion: "v1.2.2", ManifestURL: "https://example.test/release.json", ManifestSHA256: strings.Repeat("a", 64), FabricCheck: []string{"true"}, Nodes: []RolloutNode{
		{ID: "remote", Platform: "linux-amd64", Address: "ssh-build", Components: []string{"controller", "executor"}},
		{ID: "local", Platform: "darwin-arm64", Local: true, Components: []string{"controller", "executor", "provider"}},
	}}
	data, _ := json.Marshal(plan)
	parsed, err := ParseRolloutPlan(data)
	if err != nil {
		t.Fatal(err)
	}
	if parsed.RolloutOrder != "declared" || parsed.OnFabricDegraded != "pause" || orderRolloutNodes(parsed.Nodes, parsed.RolloutOrder)[0].ID != "remote" {
		t.Fatalf("invalid defaults: %#v", parsed)
	}
	plan.Nodes[0].Role = "executor"
	data, _ = json.Marshal(plan)
	if _, err = ParseRolloutPlan(data); err == nil {
		t.Fatal("mixed roles and components accepted")
	}
}

func TestSinglePlatformManifestAndPackageIdentity(t *testing.T) {
	m := Manifest{PackageID: "widget", SchemaVersion: ManifestSchema, Version: "v1.0.0", StateFormat: 1, Compatibility: Compatibility{Controller: "*", Executor: "*"}, Artifacts: map[string]Artifact{"linux-amd64": {URL: "https://example.test/widget.tar.gz", SHA256: strings.Repeat("a", 64), Format: "tar.gz"}}, Commands: map[string]string{"widget": "bin/widget"}}
	data, _ := json.Marshal(m)
	if _, err := ParseManifest(data); err != nil {
		t.Fatal(err)
	}
	client := New(testPaths(t.TempDir()))
	if _, err := client.parseManifest(data); err == nil {
		t.Fatal("foreign package identity accepted")
	}
	client.Policy.PackageID = "widget"
	if _, err := client.parseManifest(data); err != nil {
		t.Fatal(err)
	}
}

func TestFabricDegradedPausesWithoutRollbackAndCannotSkipRecovery(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX health fixture")
	}
	root := t.TempDir()
	client := New(testPaths(root))
	client.FabricCheckTimeout = 20 * time.Millisecond
	client.FabricCheckInterval = time.Millisecond
	var newManifest, newDigest string
	for _, version := range []string{"v1.0.0", "v1.0.1"} {
		script := "#!/bin/sh\nexit 0\n"
		archive, digest := makeArchive(t, root, version, script)
		manifest := makeManifest(t, root, version, archive, digest, digestBytes([]byte(script)))
		if version == "v1.0.0" {
			if err := client.Install(context.Background(), version, manifest, "", false, false, ""); err != nil {
				t.Fatal(err)
			}
		} else {
			newManifest = manifest
			newDigest, _ = digestFile(manifest)
		}
	}
	ready := filepath.Join(root, "ready")
	plan := RolloutPlan{SchemaVersion: "distributed-workbench.rollout/v1", Version: "v1.0.1", RollbackVersion: "v1.0.0", ManifestURL: newManifest, ManifestSHA256: newDigest, Nodes: []RolloutNode{{ID: "local", Platform: "darwin-arm64", Local: true, Components: []string{"controller", "executor"}}}, FabricCheck: []string{"sh", "-c", "test -f \"$1\"", "check", ready}}
	data, _ := json.Marshal(plan)
	planPath := filepath.Join(root, "plan.json")
	os.WriteFile(planPath, data, 0600)
	state, err := client.StartRollout(context.Background(), planPath)
	if err == nil || state.Status != "degraded" {
		t.Fatalf("state=%#v err=%v", state, err)
	}
	if current, _ := client.CurrentVersion(); current != "v1.0.1" {
		t.Fatalf("unexpected rollback: %s", current)
	}
	if _, err = client.ContinueRollout(context.Background(), state.ID, "retry"); err == nil {
		t.Fatal("unhealthy fabric bypassed")
	}
	os.WriteFile(ready, []byte("ready"), 0600)
	state, err = client.ContinueRollout(context.Background(), state.ID, "connectivity recovered")
	if err != nil || state.Status != "complete" {
		t.Fatalf("state=%#v err=%v", state, err)
	}
}
