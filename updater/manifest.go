package updater

import (
	"encoding/json"
	"fmt"
	"regexp"
	"sort"
	"strings"
)

const ManifestSchema = "distributed-workbench.release/v1"

var exactVersion = regexp.MustCompile(`^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$`)

type Manifest struct {
	PackageID      string              `json:"packageId,omitempty"`
	SchemaVersion  string              `json:"schemaVersion"`
	Version        string              `json:"version"`
	MinimumUpdater string              `json:"minimumUpdaterVersion,omitempty"`
	StateFormat    int                 `json:"stateFormat"`
	Status         string              `json:"status,omitempty"`
	Compatibility  Compatibility       `json:"compatibility"`
	Artifacts      map[string]Artifact `json:"artifacts"`
	Dependencies   []Dependency        `json:"dependencies,omitempty"`
	Commands       map[string]string   `json:"commands"`
	HealthCheck    *HealthCheck        `json:"healthCheck,omitempty"`
	Files          map[string]string   `json:"files,omitempty"`
}

type Compatibility struct {
	Controller string `json:"controller"`
	Executor   string `json:"executor"`
}

type Artifact struct {
	URL             string            `json:"url"`
	SHA256          string            `json:"sha256"`
	Format          string            `json:"format"`
	StripComponents int               `json:"stripComponents,omitempty"`
	Files           map[string]string `json:"files,omitempty"`
	Commands        map[string]string `json:"commands,omitempty"`
	HealthCheck     *HealthCheck      `json:"healthCheck,omitempty"`
}

type Dependency struct {
	Name       string              `json:"name"`
	Version    string              `json:"version"`
	Repository string              `json:"repository"`
	Revision   string              `json:"revision"`
	Artifacts  map[string]Artifact `json:"artifacts"`
}

type HealthCheck struct {
	Command        string   `json:"command"`
	Args           []string `json:"args,omitempty"`
	TimeoutSeconds int      `json:"timeoutSeconds"`
}

func ParseManifest(data []byte) (*Manifest, error) {
	return ParseManifestForSchema(data, ManifestSchema)
}
func ParseManifestForSchema(data []byte, schema string) (*Manifest, error) {
	var manifest Manifest
	decoder := json.NewDecoder(strings.NewReader(string(data)))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&manifest); err != nil {
		return nil, fmt.Errorf("parse manifest: %w", err)
	}
	if err := manifest.validateSchema(schema); err != nil {
		return nil, err
	}
	return &manifest, nil
}

func (m *Manifest) Validate() error { return m.validateSchema(ManifestSchema) }
func (m *Manifest) validateSchema(schema string) error {
	if m.SchemaVersion != schema {
		return fmt.Errorf("unsupported manifest schema %q", m.SchemaVersion)
	}
	if schema == ManifestSchema && strings.TrimSpace(m.PackageID) == "" {
		return fmt.Errorf("packageId is required")
	}
	if !exactVersion.MatchString(m.Version) {
		return fmt.Errorf("manifest version must be exact semver with v prefix: %q", m.Version)
	}
	if m.MinimumUpdater != "" && !exactVersion.MatchString(m.MinimumUpdater) {
		return fmt.Errorf("minimumUpdaterVersion must be exact semver")
	}
	if m.StateFormat < 1 {
		return fmt.Errorf("stateFormat must be positive")
	}
	if strings.TrimSpace(m.Compatibility.Controller) == "" || strings.TrimSpace(m.Compatibility.Executor) == "" {
		return fmt.Errorf("controller and executor compatibility ranges are required")
	}
	if m.Status == "" {
		m.Status = "supported"
	}
	if m.Status != "supported" && m.Status != "deprecated" && m.Status != "revoked" {
		return fmt.Errorf("unsupported release status %q", m.Status)
	}
	if len(m.Artifacts) == 0 {
		return fmt.Errorf("manifest needs at least one platform artifact")
	}
	for platform, artifact := range m.Artifacts {
		if err := validateArtifact(platform, artifact); err != nil {
			return err
		}
	}
	seen := map[string]bool{}
	for _, dependency := range m.Dependencies {
		if dependency.Name == "" || dependency.Version == "" || dependency.Repository == "" || dependency.Revision == "" || seen[dependency.Name] {
			return fmt.Errorf("dependencies need unique non-empty names and versions")
		}
		seen[dependency.Name] = true
		if len(dependency.Artifacts) == 0 {
			return fmt.Errorf("dependency %s needs at least one platform artifact", dependency.Name)
		}
		for platform, artifact := range dependency.Artifacts {
			if err := validateArtifact(platform, artifact); err != nil {
				return fmt.Errorf("dependency %s: %w", dependency.Name, err)
			}
		}
	}
	if len(m.Commands) == 0 {
		return fmt.Errorf("manifest commands must not be empty")
	}
	for name, path := range m.Commands {
		if name == "" || !safeRelative(path) {
			return fmt.Errorf("invalid command mapping %q -> %q", name, path)
		}
	}
	if m.HealthCheck != nil {
		if !safeRelative(m.HealthCheck.Command) || m.HealthCheck.TimeoutSeconds < 1 || m.HealthCheck.TimeoutSeconds > 300 {
			return fmt.Errorf("invalid health check")
		}
	}
	for path, digest := range m.Files {
		if !safeRelative(path) || !validSHA256(digest) {
			return fmt.Errorf("invalid file digest %q", path)
		}
	}
	return nil
}

func validateArtifact(platform string, artifact Artifact) error {
	if platform != "darwin-arm64" && platform != "linux-amd64" && platform != "windows-amd64" {
		return fmt.Errorf("unsupported artifact platform %q", platform)
	}
	if artifact.URL == "" || !validSHA256(artifact.SHA256) {
		return fmt.Errorf("artifact %s needs URL and lowercase SHA-256", platform)
	}
	if artifact.Format != "tar.gz" && artifact.Format != "zip" {
		return fmt.Errorf("artifact %s has unsupported format %q", platform, artifact.Format)
	}
	if artifact.StripComponents < 0 || artifact.StripComponents > 8 {
		return fmt.Errorf("artifact %s has invalid stripComponents", platform)
	}
	for path, digest := range artifact.Files {
		if !safeRelative(path) || !validSHA256(digest) {
			return fmt.Errorf("artifact %s has invalid file digest %q", platform, path)
		}
	}
	for name, path := range artifact.Commands {
		if name == "" || !safeRelative(path) {
			return fmt.Errorf("artifact %s has invalid command %q", platform, name)
		}
	}
	if artifact.HealthCheck != nil && (!safeRelative(artifact.HealthCheck.Command) || artifact.HealthCheck.TimeoutSeconds < 1 || artifact.HealthCheck.TimeoutSeconds > 300) {
		return fmt.Errorf("artifact %s has invalid health check", platform)
	}
	return nil
}

func safeRelative(path string) bool {
	return path != "" && !strings.HasPrefix(path, "/") && !strings.HasPrefix(path, `\`) && !strings.Contains(path, "..") && !strings.Contains(path, ":")
}

func validSHA256(value string) bool {
	if len(value) != 64 {
		return false
	}
	for _, char := range value {
		if !strings.ContainsRune("0123456789abcdef", char) {
			return false
		}
	}
	return true
}

func Platforms(m *Manifest) []string {
	values := make([]string, 0, len(m.Artifacts))
	for platform := range m.Artifacts {
		values = append(values, platform)
	}
	sort.Strings(values)
	return values
}
