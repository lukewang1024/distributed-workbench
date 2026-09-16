package updater

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

type RolloutPlan struct {
	OnFabricDegraded string        `json:"onFabricDegraded,omitempty"`
	SchemaVersion    string        `json:"schemaVersion"`
	Version          string        `json:"version"`
	ManifestURL      string        `json:"manifestUrl"`
	ManifestSHA256   string        `json:"manifestSha256"`
	RollbackVersion  string        `json:"rollbackVersion"`
	RolloutOrder     string        `json:"rolloutOrder,omitempty"`
	Nodes            []RolloutNode `json:"nodes"`
	FabricCheck      []string      `json:"fabricCheck"`
}

type RolloutNode struct {
	Components []string `json:"components,omitempty"`
	ID         string   `json:"id"`
	Platform   string   `json:"platform"`
	Address    string   `json:"address,omitempty"`
	Role       string   `json:"role,omitempty"`
	Local      bool     `json:"local,omitempty"`
}

type RolloutState struct {
	ID           string         `json:"id"`
	Plan         RolloutPlan    `json:"plan"`
	OrderedNodes []RolloutNode  `json:"orderedNodes"`
	NextNode     int            `json:"nextNode"`
	Status       string         `json:"status"`
	DegradedNode string         `json:"degradedNode,omitempty"`
	Actor        string         `json:"actor,omitempty"`
	Reason       string         `json:"reason,omitempty"`
	StartedAt    time.Time      `json:"startedAt"`
	UpdatedAt    time.Time      `json:"updatedAt"`
	Events       []RolloutEvent `json:"events"`
}

type RolloutEvent struct {
	Node   string    `json:"node,omitempty"`
	Type   string    `json:"type"`
	At     time.Time `json:"at"`
	Detail string    `json:"detail,omitempty"`
}

func ParseRolloutPlan(data []byte) (*RolloutPlan, error) {
	return ParseRolloutPlanForSchema(data, "distributed-workbench.rollout/v1")
}
func ParseRolloutPlanForSchema(data []byte, schema string) (*RolloutPlan, error) {
	var plan RolloutPlan
	decoder := json.NewDecoder(strings.NewReader(string(data)))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&plan); err != nil {
		return nil, err
	}
	if plan.SchemaVersion != schema || !exactVersion.MatchString(plan.Version) || !exactVersion.MatchString(plan.RollbackVersion) || plan.ManifestURL == "" || !validSHA256(plan.ManifestSHA256) {
		return nil, fmt.Errorf("invalid rollout plan identity")
	}
	if len(plan.Nodes) < 1 {
		return nil, fmt.Errorf("rollout plan has no nodes")
	}
	seen := map[string]bool{}
	controllers := 0
	componentNodes := 0
	locals := 0
	for _, node := range plan.Nodes {
		if node.ID == "" || seen[node.ID] {
			return nil, fmt.Errorf("rollout node ids must be unique")
		}
		seen[node.ID] = true
		if node.Platform != "darwin-arm64" && node.Platform != "linux-amd64" && node.Platform != "windows-amd64" {
			return nil, fmt.Errorf("unsupported node platform %s", node.Platform)
		}
		if len(node.Components) > 0 {
			componentNodes++
			if node.Role != "" {
				return nil, fmt.Errorf("node %s cannot mix role and components", node.ID)
			}
			seenComponents := map[string]bool{}
			for _, component := range node.Components {
				if component == "" || seenComponents[component] {
					return nil, fmt.Errorf("invalid components on node %s", node.ID)
				}
				seenComponents[component] = true
			}
		} else if node.Role == "controller" {
			controllers++
		} else if node.Role != "executor" {
			return nil, fmt.Errorf("node %s has invalid role", node.ID)
		}
		if node.Local {
			locals++
		} else if node.Address == "" {
			return nil, fmt.Errorf("remote node %s needs address", node.ID)
		}
	}
	if (componentNodes == 0 && controllers != 1) || (componentNodes != 0 && componentNodes != len(plan.Nodes)) || locals > 1 {
		return nil, fmt.Errorf("rollout needs components on every node (or legacy roles with one controller), and at most one local node")
	}
	if len(plan.FabricCheck) == 0 {
		return nil, fmt.Errorf("rollout requires a fabricCheck command")
	}
	if plan.RolloutOrder == "" {
		if componentNodes > 0 {
			plan.RolloutOrder = "declared"
		} else {
			plan.RolloutOrder = "executors-first"
		}
	}
	if plan.RolloutOrder != "declared" && plan.RolloutOrder != "executors-first" && plan.RolloutOrder != "controller-first" {
		return nil, fmt.Errorf("unsupported rolloutOrder %q", plan.RolloutOrder)
	}
	if componentNodes > 0 && plan.RolloutOrder != "declared" {
		return nil, fmt.Errorf("component nodes require declared rollout order")
	}
	if plan.OnFabricDegraded == "" {
		plan.OnFabricDegraded = "pause"
	}
	if plan.OnFabricDegraded != "pause" && plan.OnFabricDegraded != "rollback" {
		return nil, fmt.Errorf("invalid onFabricDegraded")
	}
	return &plan, nil
}

func orderRolloutNodes(nodes []RolloutNode, order string) []RolloutNode {
	if order == "declared" {
		return append([]RolloutNode(nil), nodes...)
	}

	ordered := make([]RolloutNode, 0, len(nodes))
	if order == "controller-first" {
		for _, node := range nodes {
			if node.Role == "controller" {
				ordered = append(ordered, node)
			}
		}
	}
	for _, node := range nodes {
		if node.Role == "executor" {
			ordered = append(ordered, node)
		}
	}
	if order != "controller-first" {
		for _, node := range nodes {
			if node.Role == "controller" {
				ordered = append(ordered, node)
			}
		}
	}
	return ordered
}

func (u *Updater) StartRollout(ctx context.Context, planPath string) (*RolloutState, error) {
	data, err := os.ReadFile(planPath)
	if err != nil {
		return nil, err
	}
	plan, err := ParseRolloutPlanForSchema(data, u.Policy.RolloutSchema)
	if err != nil {
		return nil, err
	}
	if err = u.preflightRolloutNodes(ctx, *plan); err != nil {
		return nil, err
	}
	lock, err := AcquireLock(u.Paths.RolloutLock(), "rollout", plan.Version)
	if err != nil {
		return nil, err
	}
	defer lock.Release()
	now := time.Now().UTC()
	state := &RolloutState{ID: fmt.Sprintf("%d-%d", now.UnixNano(), os.Getpid()), Plan: *plan, OrderedNodes: orderRolloutNodes(plan.Nodes, plan.RolloutOrder), Status: "running", Actor: os.Getenv("USER"), StartedAt: now, UpdatedAt: now}
	if err = u.writeRollout(state); err != nil {
		return nil, err
	}
	err = u.advanceRollout(ctx, state)
	return state, err
}

func (u *Updater) preflightRolloutNodes(ctx context.Context, plan RolloutPlan) error {
	for _, node := range plan.Nodes {
		version, _, err := u.readRolloutNode(ctx, node)
		if err != nil {
			return fmt.Errorf("rollout preflight failed for node %s before any install: %w", node.ID, err)
		}
		if version != plan.RollbackVersion {
			return fmt.Errorf("rollout preflight failed for node %s: installed %s, expected rollbackVersion %s", node.ID, version, plan.RollbackVersion)
		}
	}
	return nil
}

func (u *Updater) ContinueRollout(ctx context.Context, id, reason string) (*RolloutState, error) {
	if strings.TrimSpace(reason) == "" {
		return nil, fmt.Errorf("continue requires an audit reason")
	}
	state, err := u.loadRollout(id)
	if err != nil {
		return nil, err
	}
	if state.Status != "degraded" {
		return nil, fmt.Errorf("rollout %s is %s, not degraded", id, state.Status)
	}
	lock, err := AcquireLock(u.Paths.RolloutLock(), "rollout-continue", state.Plan.Version)
	if err != nil {
		return nil, err
	}
	defer lock.Release()
	if _, err := u.waitForFabric(ctx, state.Plan.FabricCheck, state.Plan.Nodes...); err != nil {
		return state, fmt.Errorf("fabric remains degraded: %w", err)
	}
	state.Status = "running"
	state.Reason = reason
	state.Actor = os.Getenv("USER")
	state.Events = append(state.Events, RolloutEvent{Type: "continued", At: time.Now().UTC(), Detail: reason})
	if err = u.writeRollout(state); err != nil {
		return nil, err
	}
	err = u.advanceRollout(ctx, state)
	return state, err
}

func (u *Updater) AbortRollout(id, reason string) (*RolloutState, error) {
	if strings.TrimSpace(reason) == "" {
		return nil, fmt.Errorf("abort requires an audit reason")
	}
	state, err := u.loadRollout(id)
	if err != nil {
		return nil, err
	}
	lock, err := AcquireLock(u.Paths.RolloutLock(), "rollout-abort", state.Plan.Version)
	if err != nil {
		return nil, err
	}
	defer lock.Release()
	if state.Status == "complete" || state.Status == "aborted" {
		return nil, fmt.Errorf("rollout is already %s", state.Status)
	}
	state.Status = "aborted"
	state.Actor = os.Getenv("USER")
	state.Reason = reason
	state.Events = append(state.Events, RolloutEvent{Type: "aborted", At: time.Now().UTC(), Detail: reason})
	return state, u.writeRollout(state)
}

func (u *Updater) RollbackRolloutNode(ctx context.Context, id, nodeID, version, reason string) (*RolloutState, error) {
	if !exactVersion.MatchString(version) || strings.TrimSpace(reason) == "" {
		return nil, fmt.Errorf("rollback-node requires exact --version and --reason")
	}
	state, err := u.loadRollout(id)
	if err != nil {
		return nil, err
	}
	var node *RolloutNode
	for index := range state.OrderedNodes {
		if state.OrderedNodes[index].ID == nodeID {
			node = &state.OrderedNodes[index]
			break
		}
	}
	if node == nil {
		return nil, fmt.Errorf("unknown rollout node %s", nodeID)
	}
	lock, err := AcquireLock(u.Paths.RolloutLock(), "rollout-rollback-node", version)
	if err != nil {
		return nil, err
	}
	defer lock.Release()
	if node.Local {
		err = u.Rollback(ctx, version, false, "")
	} else {
		var remote string
		if node.Platform == "windows-amd64" {
			remote = `powershell.exe -NoProfile -NonInteractive -Command "& \"$env:LOCALAPPDATA\workbench\bin\workbench-updater.exe\" rollback ` + shellQuotePowerShell(version) + `"`
		} else {
			remote = `"$HOME/.local/bin/workbench-updater" rollback ` + shellQuote(version)
		}
		command := exec.CommandContext(ctx, "ssh", "-o", "BatchMode=yes", "-o", "ClearAllForwardings=yes", node.Address, remote)
		var output []byte
		output, err = command.CombinedOutput()
		if err != nil {
			err = fmt.Errorf("node %s rollback failed: %s", node.ID, strings.TrimSpace(string(output)))
		}
	}
	if err != nil {
		return state, err
	}
	state.Status = "aborted"
	state.Actor = os.Getenv("USER")
	state.Reason = reason
	state.Events = append(state.Events, RolloutEvent{Node: nodeID, Type: "node-rolled-back", At: time.Now().UTC(), Detail: version + ": " + reason})
	return state, u.writeRollout(state)
}

func (u *Updater) advanceRollout(ctx context.Context, state *RolloutState) error {
	for state.NextNode < len(state.OrderedNodes) {
		node := state.OrderedNodes[state.NextNode]
		state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "install-started", At: time.Now().UTC()})
		_ = u.writeRollout(state)
		if err := u.installRolloutNode(ctx, node, state.Plan); err != nil {
			state.Status = "failed"
			state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "install-failed", At: time.Now().UTC(), Detail: err.Error()})
			_ = u.writeRollout(state)
			return u.rollbackInstalledNodes(ctx, state, state.NextNode+1, err)
		}
		installedVersion, installedDigest, err := u.readRolloutNode(ctx, node)
		if err != nil || installedVersion != state.Plan.Version || installedDigest != state.Plan.ManifestSHA256 {
			state.Status = "failed"
			detail := fmt.Sprintf("readback version=%s digest=%s: %v", installedVersion, installedDigest, err)
			state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "digest-mismatch", At: time.Now().UTC(), Detail: detail})
			_ = u.writeRollout(state)
			return u.rollbackInstalledNodes(ctx, state, state.NextNode+1, fmt.Errorf("node %s installed release readback mismatch", node.ID))
		}
		state.NextNode++
		state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "digest-verified", At: time.Now().UTC(), Detail: installedDigest})
		_ = u.writeRollout(state)
		output, err := u.waitForFabric(ctx, state.Plan.FabricCheck, state.Plan.Nodes...)
		if err != nil {
			state.Status = "degraded"
			state.DegradedNode = node.ID
			state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "fabric-degraded", At: time.Now().UTC(), Detail: strings.TrimSpace(string(output))})
			_ = u.writeRollout(state)
			failure := fmt.Errorf("node %s is locally healthy but fabric-degraded; rollout %s stopped", node.ID, state.ID)
			if state.Plan.OnFabricDegraded == "rollback" {
				return u.rollbackInstalledNodes(ctx, state, state.NextNode, failure)
			}
			return failure
		}
		state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "fabric-healthy", At: time.Now().UTC()})
		_ = u.writeRollout(state)
	}
	state.Status = "complete"
	state.DegradedNode = ""
	state.Events = append(state.Events, RolloutEvent{Type: "completed", At: time.Now().UTC()})
	return u.writeRollout(state)
}

func (u *Updater) waitForFabric(parent context.Context, command []string, nodes ...RolloutNode) ([]byte, error) {
	timeout := u.FabricCheckTimeout
	if timeout <= 0 {
		timeout = 90 * time.Second
	}
	interval := u.FabricCheckInterval
	if interval <= 0 {
		interval = 2 * time.Second
	}
	ctx, cancel := context.WithTimeout(parent, timeout)
	defer cancel()
	var output []byte
	var err error
	for {
		check := exec.CommandContext(ctx, command[0], command[1:]...)
		if len(nodes) > 0 {
			ids := make([]string, 0, len(nodes))
			for _, node := range nodes {
				ids = append(ids, node.ID)
			}
			check.Env = append(os.Environ(), "WORKBENCH_ROLLOUT_NODE_IDS="+strings.Join(ids, ","))
		}
		output, err = check.CombinedOutput()
		if err == nil {
			return output, nil
		}
		select {
		case <-ctx.Done():
			return output, fmt.Errorf("fabric readiness deadline exceeded after %s: %w", timeout, err)
		case <-time.After(interval):
		}
	}
}

func (u *Updater) rollbackInstalledNodes(ctx context.Context, state *RolloutState, count int, cause error) error {
	rollbackErrors := []string{}
	if count > len(state.OrderedNodes) {
		count = len(state.OrderedNodes)
	}
	for index := count - 1; index >= 0; index-- {
		node := state.OrderedNodes[index]
		state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "rollback-started", At: time.Now().UTC(), Detail: state.Plan.RollbackVersion})
		if err := u.rollbackNode(ctx, node, state.Plan.RollbackVersion); err != nil {
			rollbackErrors = append(rollbackErrors, fmt.Sprintf("%s: %v", node.ID, err))
			state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "rollback-failed", At: time.Now().UTC(), Detail: err.Error()})
			continue
		}
		installedVersion, _, err := u.readRolloutNode(ctx, node)
		if err != nil || installedVersion != state.Plan.RollbackVersion {
			detail := fmt.Sprintf("readback version=%s: %v", installedVersion, err)
			rollbackErrors = append(rollbackErrors, fmt.Sprintf("%s: %s", node.ID, detail))
			state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "rollback-readback-mismatch", At: time.Now().UTC(), Detail: detail})
			continue
		}
		state.Events = append(state.Events, RolloutEvent{Node: node.ID, Type: "rollback-verified", At: time.Now().UTC(), Detail: state.Plan.RollbackVersion})
	}
	state.Status = "rolled-back"
	state.Reason = cause.Error()
	_ = u.writeRollout(state)
	if len(rollbackErrors) > 0 {
		return fmt.Errorf("%w; rollback incomplete: %s", cause, strings.Join(rollbackErrors, "; "))
	}
	return fmt.Errorf("%w; installed nodes rolled back to %s", cause, state.Plan.RollbackVersion)
}

func (u *Updater) rollbackNode(ctx context.Context, node RolloutNode, version string) error {
	if node.Local {
		return u.Rollback(ctx, version, false, "")
	}
	remote := `"$HOME/.local/bin/workbench-updater" rollback ` + shellQuote(version)
	if node.Platform == "windows-amd64" {
		remote = `powershell.exe -NoProfile -NonInteractive -Command "& \"$env:LOCALAPPDATA\workbench\bin\workbench-updater.exe\" rollback ` + shellQuotePowerShell(version) + `"`
	}
	command := exec.CommandContext(ctx, "ssh", "-o", "BatchMode=yes", "-o", "ClearAllForwardings=yes", node.Address, remote)
	output, err := command.CombinedOutput()
	if err != nil {
		return fmt.Errorf("node %s rollback failed: %s", node.ID, strings.TrimSpace(string(output)))
	}
	return nil
}

func (u *Updater) readRolloutNode(ctx context.Context, node RolloutNode) (string, string, error) {
	if node.Local {
		status, err := u.Status(true)
		if err != nil || status.CurrentRecord == nil || status.Integrity != "verified" {
			return "", "", fmt.Errorf("local status verification failed: %v", err)
		}
		return status.Current, status.CurrentRecord.ManifestDigest, nil
	}
	remote := `"$HOME/.local/bin/workbench-updater" status --verify`
	if node.Platform == "windows-amd64" {
		remote = `powershell.exe -NoProfile -NonInteractive -Command "& \"$env:LOCALAPPDATA\workbench\bin\workbench-updater.exe\" status --verify"`
	}
	command := exec.CommandContext(ctx, "ssh", "-o", "BatchMode=yes", "-o", "ClearAllForwardings=yes", node.Address, remote)
	output, err := command.Output()
	if err != nil {
		return "", "", err
	}
	var status Status
	if err := json.Unmarshal(output, &status); err != nil {
		return "", "", fmt.Errorf("INVALID_EXECUTOR_RESPONSE: %w", err)
	}
	if status.CurrentRecord == nil || status.Integrity != "verified" {
		return status.Current, "", fmt.Errorf("node integrity is %s", status.Integrity)
	}
	return status.Current, status.CurrentRecord.ManifestDigest, nil
}

func (u *Updater) installRolloutNode(ctx context.Context, node RolloutNode, plan RolloutPlan) error {
	if node.Local {
		return u.Install(ctx, plan.Version, plan.ManifestURL, plan.ManifestSHA256, false, false, "")
	}
	var remote string
	if node.Platform == "windows-amd64" {
		remote = `powershell.exe -NoProfile -NonInteractive -Command "& \"$env:LOCALAPPDATA\workbench\bin\workbench-updater.exe\" install ` + shellQuotePowerShell(plan.Version) + ` --manifest ` + shellQuotePowerShell(plan.ManifestURL) + ` --manifest-sha256 ` + shellQuotePowerShell(plan.ManifestSHA256) + `"`
	} else {
		remote = `"$HOME/.local/bin/workbench-updater" install ` + shellQuote(plan.Version) + " --manifest " + shellQuote(plan.ManifestURL) + " --manifest-sha256 " + shellQuote(plan.ManifestSHA256)
	}
	command := exec.CommandContext(ctx, "ssh", "-o", "BatchMode=yes", "-o", "ClearAllForwardings=yes", node.Address, remote)
	output, err := command.CombinedOutput()
	if err != nil {
		return fmt.Errorf("node %s install failed: %s", node.ID, strings.TrimSpace(string(output)))
	}
	return nil
}

func shellQuotePowerShell(value string) string {
	return "'" + strings.ReplaceAll(value, "'", "''") + "'"
}
func (u *Updater) rolloutRoot() string { return filepath.Join(u.Paths.StateRoot, "rollouts") }
func (u *Updater) writeRollout(state *RolloutState) error {
	state.UpdatedAt = time.Now().UTC()
	if err := os.MkdirAll(u.rolloutRoot(), 0o700); err != nil {
		return err
	}
	data, _ := json.MarshalIndent(state, "", "  ")
	temporary := filepath.Join(u.rolloutRoot(), state.ID+".tmp")
	final := filepath.Join(u.rolloutRoot(), state.ID+".json")
	if err := os.WriteFile(temporary, append(data, '\n'), 0o600); err != nil {
		return err
	}
	return replacePath(temporary, final)
}
func (u *Updater) loadRollout(id string) (*RolloutState, error) {
	if id == "" || strings.ContainsAny(id, "/\\") {
		return nil, fmt.Errorf("invalid rollout id")
	}
	data, err := os.ReadFile(filepath.Join(u.rolloutRoot(), id+".json"))
	if err != nil {
		return nil, err
	}
	var state RolloutState
	if err = json.Unmarshal(data, &state); err != nil {
		return nil, err
	}
	return &state, nil
}

func (u *Updater) latestRollout() *RolloutState {
	entries, err := os.ReadDir(u.rolloutRoot())
	if err != nil {
		return nil
	}
	latest := ""
	for _, entry := range entries {
		if !entry.IsDir() && strings.HasSuffix(entry.Name(), ".json") && entry.Name() > latest {
			latest = entry.Name()
		}
	}
	if latest == "" {
		return nil
	}
	state, err := u.loadRollout(strings.TrimSuffix(latest, ".json"))
	if err != nil {
		return nil
	}
	return state
}
