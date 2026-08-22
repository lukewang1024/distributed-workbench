---
name: workbench-fabric
description: Deploy, upgrade, audit, and repair a distributed-workbench fabric from a laptop. Use when selecting POSIX or native Windows development hosts, initializing Controller and Executor services across machines, checking peer connectivity or reconnect behavior, diagnosing an offline node, or reconciling every node onto one release and topology. Do not use for product-specific builds, application publishing, UI testing, or workspace placement.
---

# Workbench Fabric

Operate the domain-neutral machine fabric. Keep product workflows in their owning adapter skill.

## Establish scope

1. Prefer a `distributed-workbench.dev/v1` `Fabric` manifest as the validated inventory contract. Validate it with `workbench fabric validate --file <path>`, then inspect the authoritative physical dialer plan with `workbench fabric plan --file <path>`. If no manifest exists, identify the laptop and SSH-config aliases explicitly selected by the user and offer to create one from `examples/fabric.yaml`.
2. Confirm that SSH is initiated from the laptop. Do not require a devbox to initiate SSH back to the laptop.
3. Treat each machine as one node with one Controller and one Executor. Treat Controller/Executor as logical roles multiplexed over one persistent peer connection per node pair.
4. Use the same release across the selected fabric. Do not perform a mixed-version rollout.

## Choose the operation

- Audit without mutation: run `scripts/bootstrap-fabric.sh --verify-only ...`.
- Repair a stuck Controller/peer chain: run `scripts/repair-fabric.sh --version VERSION --local-id ID ...`.
- Preflight a fresh selection without installing services: run `scripts/preflight-fabric.sh --version VERSION ...`.
- Prove a pinned release can parse the manifest before installing services: run `scripts/plan-release-fabric.sh <version> <manifest>`.
- Install or reconcile: run `scripts/bootstrap-fabric.sh ...`.
- Pin a release: add `--version <version>`.
- Reconfigure services around an already installed local build: add `--version <version> --skip-release-install`.
- Install an Agent-role skill bundle on a POSIX node selected by a composition layer: run `scripts/install-agent-skills.sh HOST SKILL_DIR ...`; audit it with `--verify-only`; do not choose role membership or product skills in the Fabric layer.
- Inspect a single local role or peer: use `workbench --help`, then the relevant `status` or `peer status` command. Discover current flags from command help instead of reproducing them here.

Before an installing run, report the selected nodes and version. Installation, upgrade, service replacement, or topology changes require explicit user intent; diagnosis defaults to `--verify-only`.

Keep the Fabric manifest domain-neutral: node identities, platform, architecture, SSH alias, allow roots, initiator, and topology belong here; product adapters, workflows, profiles, credentials, and application paths do not. The current shell bootstrap is a compatibility executor, not a general manifest reconciler: verify that the composition layer has translated every selected node and policy without loss. In particular, do not claim custom node identities or allow roots were applied unless the executor explicitly supports them.

## Execute from the laptop

Run the repository's `scripts/bootstrap-fabric.sh` as the deterministic authority. Do not recreate its SSH, launchd, systemd, Windows Service, registration, or verification steps in ad-hoc shell commands.

The bootstrap must remain the only component that:

- installs and supervises node-local Controller and Executor services;
- chooses the physical dialer for each node pair;
- establishes the SSH-stdio framed peer transport;
- downloads and verifies POSIX release artifacts on the laptop, then stages them over SSH so remote nodes do not require GitHub access;
- registers both logical roles through node-local proxies;
- verifies Controller routes in both logical directions;
- installs composition-selected Agent skills atomically with recoverable remote backups;
- restarts installed peers and proves recovery with a higher connection generation.

Do not add SSH socket forwarding, expose Unix sockets across machines, or model the two logical directions as separate transports. Native Windows nodes use Named Pipes locally and native OpenSSH remotely; do not require WSL or MSYS2.

## Diagnose failures

Classify the first failing invariant before changing anything:

- SSH or host identity: fix laptop-to-target reachability or selected alias.
- Local role health: inspect that node's native supervisor and Controller/Executor status.
- Peer not ready: inspect the physical dialer's peer status and supervisor log.
- One logical direction fails: treat it as role registration/routing failure, not a need for reverse SSH.
- Reconnect generation does not increase: repair peer supervision or reconnect state.
- Product capability fails after the fabric is healthy: stop and hand off to the product adapter skill.

Never bypass an unhealthy Controller by directly running a product operation on a remote host.

## Report

Return the selected release, node identities, platform per node, peer readiness, both logical route directions, reconnect result, and any remaining failing invariant. Do not claim success from service installation alone.

## Verification

- Every selected node reports one healthy Controller and one healthy Executor.
- Every node pair has one ready peer and real Controller calls succeed in both logical directions.
- An installing run proves reconnect with a higher generation; a read-only audit does not restart services.
