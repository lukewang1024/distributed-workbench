# Tunnel observations and stale process records

`tunnel.get`, `tunnel.list`, and `tunnel.ensure` reconcile the recorded process
against its OS birth identity. PID existence alone does not establish identity.
New process records persist that identity; legacy records without it remain
unverified and cannot authorize signaling a PID. An identity mismatch invalidates
the old process record without signaling the replacement process.

Local forwards report live TCP reachability, listener ownership, and a temporary
bind probe separately in `observation`. `binding.state` is `available`, `in-use`,
or `unknown`. Permission errors are unknown, not occupancy evidence. A ready
observation requires both a verified process and its listener on the requested
endpoint. Remote-forward bind endpoints are not local and are reported unknown.
TCP readiness does not prove the forwarded application is healthy.

Observations have `checkedAt`, `expiresAt`, and `ttlMs` (5,000 ms). Every query
probes again; the TTL is the maximum lifetime of the returned evidence, not a
reason to skip probing. Consumers must requery after expiry. A saved readiness
success never establishes current readiness. Configuration and desired state
are durable intent, not observations; expiry does not stop healthy services or
delete history. Legacy identity evidence cannot be recovered from a PID alone.

`TUNNEL_CONFLICT` means a verified live process has a different definition. Its
error details include fresh observations of the existing and requested binding;
it does not mean the requested port is occupied. Keep the requested port unless
the caller explicitly allows alternatives. Do not kill an unverified PID or
interpret `PROCESS_IDENTITY_UNKNOWN` as a port conflict. SSH binding with
`ExitOnForwardFailure` remains authoritative at startup because a preflight
probe cannot reserve the port against a concurrent binder.

Controller readiness waits finish with a fresh `tunnel.get`; a successful
historical readiness result must not be turned into a fabricated ready view.

Exited processes, including unreaped Unix zombies, are failed records even
when `kill(pid, 0)` still succeeds. Both Linux and macOS check the OS process
state before treating a PID as live. Managed children are waited for in the
background so their exit does not accumulate zombies in a long-running
Executor. Existing zombie records are reconciled on load or observation;
desired-running tunnels can then use the normal restart path. No PID signaling,
record deletion, alternate tunnel ID, or weaker identity check is needed.

For `PROCESS_IDENTITY_UNKNOWN`, inspect both the recorded birth identity and
the OS state on the owning Executor. A live process with absent/unreadable
identity still requires investigation and must not be signaled. An exited
process must not remain permanently running/unknown merely because its PID
has not yet been reaped. Keep process identity, port binding, and application
HTTP health as separate evidence.
