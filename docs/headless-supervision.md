# Headless Linux node supervision

`scripts/supervise-node.py` manages an isolated Controller and Executor through an
existing user-accessible `supervisord`. It installs no desktop environment or
computer-use runtime. Python 3.9+ and a compatible supervisor are prerequisites;
the Go supervisord 0.6.8 implementation is the initial tested backend.

Supply a JSON file containing exactly these fields:

```json
{
  "nodeId": "cli-node",
  "binary": "/opt/workbench/releases/pinned/workbench",
  "supervisor": "/usr/local/bin/supervisord",
  "stateRoot": "/home/developer/.local/state/dwb-cli-node",
  "allowRoots": ["/home/developer/src"],
  "peers": [
    {
      "id": "gateway",
      "sshAlias": "gateway-ssh",
      "binary": "/opt/workbench/bin/workbench",
      "stateRoot": "/srv/workbench/state"
    }
  ]
}
```

Run as the state-directory owner:

```sh
python3 scripts/supervise-node.py --file node.json plan
python3 scripts/supervise-node.py --file node.json ensure
python3 scripts/supervise-node.py --file node.json status
```

`ensure` uses a nonblocking per-node lock, preserves identical files, starts the
private supervisor if its control socket is not listening, and verifies both
RPC endpoints, node identity, configured roots and local Executor registration.
A stale socket is not a healthy process. Repeated healthy calls do not restart
children or re-register the Executor. Content and binary digest drift while the
supervisor is running is rejected: drain work, run `stop`, then apply the new
configuration. `stop` is an explicit shutdown, not a drain operation.
Programs explicitly use `stopsignal=TERM` and a ten-second stop timeout. The
helper stops each child before shutting down the supervisor; the initial Go
backend otherwise used KILL when no stop signal was specified. This establishes
the signal/timeout policy, not application-level task draining guarantees.

`peers` is optional. Each entry starts one reconnecting SSH-stdio transport in the
same supervisor. Both Controller and Executor registrations are reconciled in
both directions, with endpoint conflicts rejected. Status checks include a
real reverse Controller call; a ready status file alone is insufficient.
Unchanged apply preserves peer PIDs and generations. Stop handles peers before
the local roles. Authentication/SSH aliases must already be configured by the
deployment owner.

`scripts/bootstrap-fabric.sh --headless-file node.json plan|ensure|status|stop`
exposes the same helper through the bootstrap entry point. This is an additive
headless attachment contract; the full-mesh manifest reconciler remains separate.

The deployment owner must supply an every-start hook calling `ensure` and, where
available, a best-effort stop hook calling `stop` as the same OS user. The helper
does not change host lifecycle configuration or keep a stopped machine awake.
It does not install a supervisor watchdog; if the supervisor itself exits, the
next external `ensure` invocation is required.

The helper does not create SSH credentials or claim full-mesh membership. A leaf
can explicitly use its gateway Controller's registered targets with:

```sh
python3 scripts/call-via.py --node-state /path/to/installed.json \
  --gateway gateway executor.call '{"executorId":"desktop-native","action":"status","params":{}}'
```

Calls are not automatically replayed. Offline targets retain their normal RPC
errors; no alternate gateway is selected implicitly.

`scripts/restricted-peer.py` supports a deployment-owned `authorized_keys`
`restrict,command="..."` entry. Its config binds `binary`, `nodeId`, `peerId` and
`stateRoot`. It accepts exactly the corresponding `peer accept` argument vector
or that binary's `--version`; other commands, identities and sockets are denied.
The deployment owner enrolls/revokes the public key and protects the policy
file. This restricts the SSH entry point; the resulting peer still has the
trusted Fabric's Controller/Executor capabilities.
