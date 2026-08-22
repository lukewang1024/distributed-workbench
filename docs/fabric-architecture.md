# Multi-node fabric architecture

## Role and transport boundaries

A machine is a node. Each node normally runs one Controller and one Executor.
Those are logical protocol roles, not owners of separate network connections.
For each node pair there is one persistent, full-duplex `PeerConnection` that
multiplexes both directions:

```text
Controller A -> Executor B       Controller B -> Executor A
Controller A <-> Controller B   control/events in either direction
```

The physical dial direction is independent of the logical request direction.
The laptop always dials a devbox because SSH cannot be initiated in the reverse
direction. Devbox pairs choose one deterministic dialer. A request from a role
on the accepting node travels back over the already-open connection; it does
not require a second SSH login.

## Why the transport does not use socket forwarding

SSH `LocalForward`/`RemoteForward` is useful for a quick Unix-only prototype,
but is the wrong fabric contract:

- it exposes one forwarding rule per role/direction instead of one node link;
- inherited SSH configuration, stale socket paths, path-length limits and
  reconnect ordering can make a healthy SSH session expose broken routes;
- Unix socket permissions and deletion semantics leak into the cross-node
  protocol;
- OpenSSH cannot forward a Windows Named Pipe, so it prevents a native Windows
  node from using the same architecture.

The implementation therefore invokes `workbench peer accept` over `ssh -T` and
carries newline-framed protocol messages on SSH stdin/stdout. A mandatory hello
handshake verifies the peer protocol version, expected node identity, and the
Controller/Executor role manifest before the connection becomes ready. Request
frames include a target role and correlation ID. Both peers expose node-local
proxy endpoints, but those endpoints never cross the network boundary.

The peer connection is the control channel. Large artifact bytes continue to
use the negotiated artifact-transfer capability (for example rsync) and only
their typed metadata and task events travel through RPC; this prevents a large
copy from head-of-line blocking control messages.

SSH supplies authentication, encryption, host identity and reachability. The
peer runtime supplies multiplexing, request correlation, health and reconnect.
This also avoids interference from user forwarding rules by setting
`ClearAllForwardings=yes`.

## Platform boundary

The wire protocol is identical on every operating system. Only node-local IPC
and service supervision vary:

| Platform | Local IPC | Supervisor | Remote entry |
| --- | --- | --- | --- |
| macOS | Unix socket | launchd | native OpenSSH client/server |
| Linux | Unix socket | systemd user service | native OpenSSH |
| Windows | Named Pipe | Windows Service | native OpenSSH Server |

The Rust local-socket abstraction maps the same endpoint API to Unix sockets or
Windows Named Pipes. A Windows dial target uses `peer connect --remote-platform windows`
so the SSH command invokes the native executable through PowerShell. The
Windows node itself does not require WSL or MSYS2.

WSL2 may run a separate Linux node when Linux behavior is itself required, but
it should have its own node identity and lifecycle. WSL1 is compatibility-only:
its syscall model and service lifecycle are a poor base for a persistent node.
MSYS2 is suitable for interactive Unix tools and bootstrap scripts, not for the
daemon, IPC, or process ownership boundary.

## Lifecycle and health

macOS peers run as launchd agents, Linux peers as systemd user services, and
Windows roles as Windows Services. The peer runtime owns reconnect with bounded
exponential backoff and SSH keepalives. Its status records a stable connection
ID and a generation that increases after reconnect.

`ready` means the framed transport is established and both remote roles answer
their health RPC. Controller and Executor health remains independently
observable; transport availability alone never grants write authority.

`bootstrap-fabric.sh` accepts POSIX SSH aliases directly and native Windows
aliases as `windows:HOST`. It installs platform-native supervisors and builds a
single deterministic connection for every selected pair. A Windows service
that is selected as a dialer needs machine-level OpenSSH credentials; inbound
laptop-to-Windows links do not.

Every run performs real federated Controller calls in both logical directions
for every node pair; it does not infer reachability from registration records.
An installing run also restarts every selected peer once, requires its
generation to increase, and repeats the route probes after reconnection.
`--verify-only` retains the bidirectional calls but deliberately skips the
restart so that the audit is read-only.

## Controller state ownership

Controllers are federated routers, not replicas of a shared multi-writer JSON
database. A workspace session and its leases are owned by one home Controller.
Other Controllers route session-scoped requests to that authority. Moving
ownership uses an explicit two-phase handoff: the old home first persists a
pending/quiescent state, transfers the session-owned task/artifact/generation/
transaction/agent/handoff bundle, and only then commits the new home with a
higher authority epoch. The operation is retryable after interruption; copying
whole Controller state files between nodes is unsupported. Controller identity
is persisted across restarts. A per-session gate serializes the route decision
and handoff boundary, so an operation that raced with handoff either completes
before quiescence or observes the persisted pending state; unrelated sessions
remain concurrent. Every mutating capability carries the Controller
ID and lease fence for all locked resources. The session epoch forms the high
bits of that execution fence, so a new home always supersedes its predecessor.
Executors persist the highest accepted fence and reject stale or same-fence
requests from a different Controller.

## Bootstrap invariant

For every selected node, bootstrap must prove that both local roles are healthy,
each node pair has one ready peer, both logical directions work, laptop-devbox
routes work without reverse SSH, and restarting a peer restores all routes with
a higher generation.
